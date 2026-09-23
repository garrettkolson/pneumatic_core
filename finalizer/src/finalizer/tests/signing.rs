//! Signature-intake tests: preload, `Sign` handling, the C1 voter-auth
//! suite (forged/missing envelope, unregistered voter, bad inner signature,
//! quorum counting, snapshot-based stake), and optimistic first-sign.
use super::helpers::*;
use super::super::*;

#[tokio::test]
async fn test_handle_preload() {
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer(pending_registry);

    let tx = Transaction {
        id: "preload_tx".to_string(),
        action: "Transfer".to_string(),
        token_id: vec![0, 1, 2],
        bid: None,
        sequence_number: 1,
        sender: vec![],
        receiver: vec![],
        amount: Some(50),
        timestamp: 1000,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let body = serialize_to_bytes_rmp(&tx).unwrap();
    let message = Message {
        chain_id: "test_env".to_string(),
        action: String::from("Preload"),
        body,
        signature: vec![],
        public_key: vec![9, 8, 7],
        stake_set: None,
    };

    let result = finalizer.handle_preload(&message).await;
    assert!(result.is_ok());
    assert_eq!(finalizer.preload_task_count().await, 1);
}

#[tokio::test]
async fn test_handle_signature_adds_to_collector() {
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry,
        Arc::new(StubDataProvider::new()),
    );

    let message =
        build_signed_sign_message("test_env", b"test_tx_001", vec![1, 2, 3], 10, &voter);

    let result = finalizer.handle_signature(&message).await;
    // OPTIMISTIC: First (authenticated) signature triggers immediate
    // optimistic finalize, which cleans up the signature registry. Count is
    // 0 after finalize.
    assert!(result.is_ok());
    assert_eq!(finalizer.signature_count("test_tx_001"), 0);
}

#[tokio::test]
async fn test_handle_signature_optimistic_first_sig() {
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry,
        Arc::new(StubDataProvider::new()),
    );

    let message =
        build_signed_sign_message("test_env", b"test_tx_001", vec![1, 2, 3], 10, &voter);

    let result = finalizer.handle_signature(&message).await;
    // First authenticated signature → optimistic finalize succeeds.
    assert!(result.is_ok());
}

// --- Phase 1.4 / audit finding C1 regression tests -----------------------
//
// Every test below asserts on what the *fix* restores and on what the
// pre-fix code (which trusted the self-declared `message.public_key`)
// violated. With an empty pending registry, an optimistic finalize fails
// before cleanup, so an admitted signature persists in the registry for
// inspection — and a rejected voter's signature never enters it.
//
// Each fails WITHOUT the fix: the old `handle_signature` never verified the
// envelope signature, never consulted the registry, never verified the inner
// signature, and never stamped stake from the snapshot.

#[tokio::test]
async fn forged_voter_key_rejected() {
    // The body is signed by `attacker`, but the message claims a *registered*
    // voter's public key. Without the fix, `handle_signature` trusted the
    // claimed `public_key` and admitted the signature for that voter.
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let attacker = NodeIdentity::generate_in_memory();
    let voter_pk = voter.ed25519.public_key().expect("voter public key");
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry,
        Arc::new(StubDataProvider::new()),
    );

    let inner = voter.ed25519.sign_data(&[1, 2, 3]).expect("voter signs inner");
    let sig = TransactionSignature {
        transaction_id: b"forged_tx".to_vec(),
        env_id: b"test_env".to_vec(),
        transaction_hash: vec![1, 2, 3],
        signature: inner,
        current_stake: 10,
    };
    let body = serialize_to_bytes_rmp(&sig).expect("serialize inner");
    // Envelope signed by the attacker, claiming the voter's key.
    let message = Message {
        chain_id: "test_env".to_string(),
        action: "Sign".to_string(),
        body,
        signature: attacker.ed25519.sign_data(&Vec::new()).expect("attacker signs"),
        public_key: voter_pk.clone(),
        stake_set: None,
    };

    let result = finalizer.handle_signature(&message).await;
    assert!(result.is_err());
    assert!(
        matches!(result.unwrap_err(), pneumatic_core::errors::PneumaticError::CryptoError(_))
    );
    // The forged identity must not have been admitted to the registry.
    assert!(
        finalizer
            .signature_registry
            .get_transaction_registry("forged_tx")
            .is_none(),
        "attacker impersonating a registered voter must not be admitted"
    );
}

#[tokio::test]
async fn missing_or_invalid_envelope_signature_rejected() {
    // A `Sign` message with an empty envelope signature. Without the fix the
    // handler ignored the signature entirely and cached the self-declared key.
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry,
        Arc::new(StubDataProvider::new()),
    );

    let mut message =
        build_signed_sign_message("test_env", b"nosig_tx", vec![1, 2, 3], 10, &voter);
    message.signature = Vec::new();

    let result = finalizer.handle_signature(&message).await;
    assert!(result.is_err());
    assert!(
        matches!(result.unwrap_err(), pneumatic_core::errors::PneumaticError::CryptoError(_))
    );
    assert!(
        finalizer
            .signature_registry
            .get_transaction_registry("nosig_tx")
            .is_none(),
        "a message with no verifiable signature must not be admitted"
    );
}

#[tokio::test]
async fn non_registered_voter_rejected() {
    // A valid envelope, but the signer is not registered as any node — the
    // core C1 rejection. Without the fix the self-declared key was silently
    // admitted into the check-or-create signature registry.
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory(); // deliberately NOT registered
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry,
        Arc::new(StubDataProvider::new()),
    );

    let message = build_signed_sign_message("test_env", b"noreg_tx", vec![1, 2, 3], 10, &voter);

    let result = finalizer.handle_signature(&message).await;
    assert!(result.is_err());
    assert!(
        matches!(result.unwrap_err(), pneumatic_core::errors::PneumaticError::Registry(_))
    );
    assert!(
        finalizer
            .signature_registry
            .get_transaction_registry("noreg_tx")
            .is_none(),
        "an unregistered voter must not pollute the signature registry"
    );
}

#[tokio::test]
async fn bad_inner_signature_rejected() {
    // Registered Executor and a valid envelope, but the inner
    // `TransactionSignature.signature` does not verify over `transaction_hash`.
    // Without the fix the inner signature was never verified at all.
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry,
        Arc::new(StubDataProvider::new()),
    );

    // Inner signature verifies over [9,9,9], but claims transaction_hash = [1,2,3].
    let inner = voter.ed25519.sign_data(&vec![9, 9, 9]).expect("inner signs wrong hash");
    let body = serialize_to_bytes_rmp(&TransactionSignature {
        transaction_id: b"bad_inner_tx".to_vec(),
        env_id: b"test_env".to_vec(),
        transaction_hash: vec![1, 2, 3],
        signature: inner,
        current_stake: 10,
    })
    .expect("serialize inner");
    // Envelope itself is a valid signature of `body` by the registered voter.
    let message = Message::signed(String::from("test_env"), "Sign", body, None, &voter).expect("sign envelope");

    let result = finalizer.handle_signature(&message).await;
    assert!(result.is_err());
    assert!(
        matches!(result.unwrap_err(), pneumatic_core::errors::PneumaticError::CryptoError(_))
    );
    assert!(
        finalizer
            .signature_registry
            .get_transaction_registry("bad_inner_tx")
            .is_none(),
        "a voter whose inner signature does not verify must not be admitted"
    );
}

#[tokio::test]
async fn quorum_counts_only_verified_registered_voters() {
    // Three voters: A and B are registered Executors, C is not. A and B are
    // admitted (and persist because the empty-pending optimistic path fails
    // before cleanup); C is rejected at the auth gate. Without the fix all
    // three would be admitted via the self-declared key.
    let node_registry = make_test_node_registry();
    let a = NodeIdentity::generate_in_memory();
    let b = NodeIdentity::generate_in_memory();
    let c = NodeIdentity::generate_in_memory(); // unregistered
    register_executor(&node_registry, &a);
    register_executor(&node_registry, &b);
    let a_pk = a.ed25519.public_key().expect("a public key");
    let b_pk = b.ed25519.public_key().expect("b public key");
    let c_pk = c.ed25519.public_key().expect("c public key");
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry,
        Arc::new(StubDataProvider::new()),
    );

    let tx = b"quorum_tx";
    let sig_a = build_signed_sign_message("test_env", tx, vec![1, 2, 3], 10, &a);
    let sig_b = build_signed_sign_message("test_env", tx, vec![1, 2, 3], 10, &b);
    let sig_c = build_signed_sign_message("test_env", tx, vec![1, 2, 3], 10, &c);

    // A: admitted, then optimistic fails (no pending tx) — signature persists.
    let _ = finalizer.handle_signature(&sig_a).await;
    // B: admitted (quorum-accumulate path, no optimistic).
    let _ = finalizer.handle_signature(&sig_b).await;
    // C: rejected at the auth gate — never reaches the registry.
    assert!(finalizer.handle_signature(&sig_c).await.is_err());

    let registry = finalizer
        .signature_registry
        .get_transaction_registry("quorum_tx")
        .expect("transaction entry exists");
    assert_eq!(
        registry.len(),
        2,
        "only the two registered+verified voters count toward quorum"
    );
    assert!(registry.contains_key(&a_pk));
    assert!(registry.contains_key(&b_pk));
    assert!(
        !registry.contains_key(&c_pk),
        "an unregistered voter must not advance the count"
    );
}

#[tokio::test]
async fn current_stake_comes_from_snapshot_not_message() {
    // The voter injects a bogus current_stake (u64::MAX). The fix stamps the
    // voter's real stake from the epoch snapshot before admitting the
    // signature; without the fix the self-reported stake would be trusted.
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let voter_pk = voter.ed25519.public_key().expect("voter public key");
    let pending_registry = make_test_pending_registry();
    let data_provider = Arc::new(
        StubDataProvider::new()
            .with_stake_snapshot(0, make_stake_set(vec![(voter_pk.clone(), 100)])),
    );
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry,
        data_provider,
    );

    let message = build_signed_sign_message("test_env", b"stake_tx", vec![1, 2, 3], u64::MAX, &voter);
    let _ = finalizer.handle_signature(&message).await;

    let registry = finalizer
        .signature_registry
        .get_transaction_registry("stake_tx")
        .expect("transaction entry exists");
    let stored = registry.get(&voter_pk).expect("voter signature persisted");
    assert_eq!(
        stored.current_stake, 100,
        "stake must be taken from the epoch snapshot"
    );
    assert_ne!(
        stored.current_stake,
        u64::MAX,
        "a self-reported stake from the message must never be trusted"
    );
}
