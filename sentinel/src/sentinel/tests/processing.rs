//! Processing tests for the Sentinel, moved from `crate::sentinel::tests`.

use super::helpers::*;
use super::super::*;


// --- Sentinel creation and behavior ---

#[test]
fn sentinel_creation_succeeds() {
    let (sentinel, _registry) = make_sentinel_fixture();
    // Just verify it was constructed without panic
    let _ = sentinel;
}


// --- on_data_received routing ---

#[test]
fn on_data_received_unknown_action_returns_error() {
    let (sentinel, _registry) = make_sentinel_fixture();
    let msg = Message {
        chain_id: "test".into(),
        action: "Zzzz".into(),
        body: vec![],
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_err());
    match result.unwrap_err() {
        SentinelError::UnknownAction(action) => assert_eq!(action, "Zzzz"),
        _ => panic!("expected UnknownAction"),
    }
}


#[test]
fn on_data_received_process_with_valid_body_no_encoding_error() {
    let (sentinel, _registry) = make_sentinel_fixture();
    let msg = Message {
        chain_id: "test".into(),
        action: "Process".into(),
        body: serialize_to_bytes_rmp(&Transaction {
            id: "test_tx".into(),
            action: "Transfer".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: vec![1],
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        }).unwrap(),
        signature: vec![1, 2, 3],
        public_key: vec![4, 5, 6],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    // Should not return Encoding error (may fail on other paths, but that's OK)
    match result {
        Err(SentinelError::Encoding(_)) => panic!("should not be Encoding error"),
        _ => {} // any other error is fine (validation, registry, etc.)
    }
}


// --- Phase 3.1 (AUDIT C3): sender-authentication regression tests ---

/// Build a "Process" message whose transaction carries a valid sender signature
/// (signed by `sender_pk`) and whose envelope sender is `sender_pk`.
fn c3_process_message(sender_pk: Vec<u8>, tx: Transaction) -> Message {
    Message {
        chain_id: "test".into(),
        action: "Process".into(),
        body: serialize_to_bytes_rmp(&tx).unwrap(),
        signature: vec![],
        public_key: sender_pk,
        stake_set: None,
    }
}


/// Sign `tx`'s canonical bytes with `identity`, returning a C3-valid transaction.
fn c3_sign(tx: &mut Transaction, identity: &NodeIdentity) {
    let canonical = tx.canonical_signature_bytes().expect("canonical transaction bytes");
    tx.sender_signature = identity.ed25519.sign_data(&canonical).expect("sender signs");
}


#[test]
fn process_tx_with_valid_sender_signature_accepted() {
    let (sentinel, _registry) = make_sentinel_fixture();
    // A real sender signs the canonical transaction bytes.
    let sender_identity = NodeIdentity::generate_in_memory();
    let sender_pk = sender_identity.ed25519.public_key().expect("sender public key");

    let mut tx = Transaction {
        id: "c3_valid".into(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: sender_pk.clone(),
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    c3_sign(&mut tx, &sender_identity);

    let msg = c3_process_message(sender_pk, tx);
    // The C3 gate must pass: the result may be Ok or a downstream stub error, but
    // never a sender-authentication rejection.
    let result = sentinel.handle_process_request(msg);
    assert!(
        !matches!(
            result,
            Err(SentinelError::UnauthenticatedSubmitter(_))
                | Err(SentinelError::InvalidSenderSignature(_))
        ),
        "valid sender signature must pass the C3 gate, got {result:?}"
    );
}


#[test]
fn unauthorized_submitter_debit_is_rejected() {
    let (sentinel, _registry) = make_sentinel_fixture();
    // HEADLINE C3: a different node submits a transaction debiting account X. Even
    // though X really signed the payload, the node submitting it is not X's node, so
    // the binding check must reject.
    let victim = NodeIdentity::generate_in_memory();
    let victim_pk = victim.ed25519.public_key().expect("victim public key");

    let attacker = NodeIdentity::generate_in_memory();
    let attacker_pk = attacker.ed25519.public_key().expect("attacker public key");

    let mut tx = Transaction {
        id: "c3_unauth".into(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: victim_pk.clone(),
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    // The payload really was authorized by victim, so the signature check passes —
    // only the binding check (network submitter != account) can stop this.
    c3_sign(&mut tx, &victim);

    let msg = c3_process_message(attacker_pk, tx);
    match sentinel.handle_process_request(msg) {
        Err(SentinelError::UnauthenticatedSubmitter(_)) => {}
        other => panic!("expected UnauthenticatedSubmitter, got {other:?}"),
    }
}


#[test]
fn forged_sender_signature_rejected() {
    let (sentinel, _registry) = make_sentinel_fixture();
    // Binding holds (envelope sender == tx.sender) but the signature is empty/forged.
    let sender = NodeIdentity::generate_in_memory();
    let sender_pk = sender.ed25519.public_key().expect("sender public key");

    let tx = Transaction {
        id: "c3_forged".into(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: sender_pk.clone(),
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![], // empty -> verify returns Ok(false)
    };

    let msg = c3_process_message(sender_pk, tx);
    match sentinel.handle_process_request(msg) {
        Err(SentinelError::InvalidSenderSignature(_)) => {}
        other => panic!("expected InvalidSenderSignature, got {other:?}"),
    }
}


#[test]
fn sender_signature_does_not_cross_accounts() {
    let (sentinel, _registry) = make_sentinel_fixture();
    // A signature valid for account A must not authorize a payload claiming sender == B.
    let a = NodeIdentity::generate_in_memory();
    let a_pk = a.ed25519.public_key().expect("a public key");
    let b_claim = vec![7, 7, 7, 7];

    let mut tx = Transaction {
        id: "c3_cross".into(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: b_claim.clone(), // claims to be B
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    // A signs the canonical bytes; the binding (envelope == "B" == tx.sender) holds,
    // but verifying A's signature against "B" fails.
    c3_sign(&mut tx, &a);

    let msg = c3_process_message(b_claim, tx);
    match sentinel.handle_process_request(msg) {
        Err(SentinelError::InvalidSenderSignature(_)) => {}
        other => panic!("A's signature cannot authorize a tx claiming sender==B, got {other:?}"),
    }
}


#[test]
fn on_data_received_clear_removes_from_registry() {
    let (sentinel, registry) = make_sentinel_fixture();
    // Register a transaction first
    registry.register_pending("tx_clear".into()).unwrap();
    assert!(registry.contains("tx_clear"));

    // Send a "Clear" message with serialized tx_id
    let msg = Message {
        chain_id: "test".into(),
        action: "Clear".into(),
        body: serialize_to_bytes_rmp(&"tx_clear".to_string()).unwrap(),
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_ok());
    assert!(!registry.contains("tx_clear"));
}


// --- T08 integration: self-signed token flow through sentinel ---

#[test]
fn sentinel_self_signed_token_flow_end_to_end() {
    let (_sentinel, registry) = make_sentinel_fixture();
    // Mint an owner-operated token through the real path (AUDIT 5.9): the owner
    // is recorded as a hex string in metadata, so the transaction-level
    // `SelfSigned` spec can hex-decode it and check `sender == owner`.
    let token = TokenFactory::mint_user_token(b"alice".to_vec(), vec![1], "test".into())
        .expect("token mints");

    // Create a self-signed transaction (sender == owner)
    let tx = Transaction {
        id: "tx_self_signed".into(),
        action: "Transfer".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: b"alice".to_vec(),
        receiver: vec![],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };

    // Validate with SelfSigned spec directly
    let spec = SelfSignedBlockValidatorSpec::new();
    let env = make_test_env_data();
    let validation_result = spec.validate(&tx, &token, &env).unwrap();
    assert!(validation_result.is_valid);

    // Create PendingTransaction and transition to Validated
    let tx_id = tx.id.clone();
    let mut pt = PendingTransaction::new(tx_id.clone(), TransactionState::Pending);
    pt.transition_to_validated(tx.clone(), validation_result);
    registry.add_transaction(tx_id.clone(), pt).unwrap();

    // Verify the sentinel sees the transaction as validated
    let validation = registry.get_validation_result(&tx_id).unwrap();
    assert!(validation.is_valid);
}


// --- compute_gas_used tests ---

#[test]
fn compute_gas_used_with_zero_amount_returns_base_cost() {
    let validator = TransactionValidator::new(
        Arc::new(make_test_env_data()),
        Arc::new(DefaultDataProvider::new()),
    );
    let tx = Transaction {
        id: "test".into(),
        action: "Process".into(),
        token_id: vec![],
        bid: None,
        sequence_number: 1,
        sender: vec![1],
        receiver: vec![2],
        amount: Some(0),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let gas = validator.compute_gas_used(&tx);
    // base_cost=1, amount=0, multiplier=1.0 → 1 + 0 = 1
    assert_eq!(gas, 1);
}


#[test]
fn compute_gas_used_preload_with_amount_applies_multiplier() {
    let validator = TransactionValidator::new(
        Arc::new(make_test_env_data()),
        Arc::new(DefaultDataProvider::new()),
    );
    let tx = Transaction {
        id: "test".into(),
        action: "Preload".into(),
        token_id: vec![],
        bid: None,
        sequence_number: 1,
        sender: vec![1],
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let gas = validator.compute_gas_used(&tx);
    // base_cost=1, amount=100, Preload multiplier=2.0 → 1 + 200 = 201
    assert_eq!(gas, 201);
}


#[test]
fn compute_gas_used_unknown_action_defaults_to_one() {
    let validator = TransactionValidator::new(
        Arc::new(make_test_env_data()),
        Arc::new(DefaultDataProvider::new()),
    );
    let tx = Transaction {
        id: "test".into(),
        action: "UnknownAction".into(),
        token_id: vec![],
        bid: None,
        sequence_number: 1,
        sender: vec![1],
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let gas = validator.compute_gas_used(&tx);
    // base_cost=1, amount=100, unknown multiplier=1.0 → 1 + 100 = 101
    assert_eq!(gas, 101);
}


// --- TransactionNotifier tests ---

#[test]
fn transaction_notifier_send_to_executors_does_not_panic() {
    let node_registry = make_test_node_registry();
    let config = make_test_config();
    let notifier = TransactionNotifier::new(config, node_registry);
    let env = make_test_env_data();
    let tx = Transaction {
        id: "test_tx".into(),
        action: "Preload".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: vec![1],
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    // Should succeed (spawns async task; no nodes registered means no sends)
    let result = notifier.send_to_executors_for_preload(&tx, &env);
    assert!(result.is_ok());
}


#[test]
fn transaction_notifier_send_to_finalizer_does_not_panic() {
    let node_registry = make_test_node_registry();
    let config = make_test_config();
    let notifier = TransactionNotifier::new(config, node_registry);
    let env = make_test_env_data();
    let tx = Transaction {
        id: "test_tx".into(),
        action: "Preload".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: vec![1],
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let result = notifier.send_to_finalizer_for_preload(&tx, b"finalizer_key", &env);
    assert!(result.is_ok());
}


#[test]
fn transaction_notifier_notify_clear_does_not_panic() {
    let node_registry = make_test_node_registry();
    let config = make_test_config();
    let notifier = TransactionNotifier::new(config, node_registry);
    let env = make_test_env_data();
    let result = notifier.notify_clear_to_process("tx_123", &env);
    assert!(result.is_ok());
}


#[test]
fn transaction_notifier_notify_delete_does_not_panic() {
    let node_registry = make_test_node_registry();
    let config = make_test_config();
    let notifier = TransactionNotifier::new(config, node_registry);
    let env = make_test_env_data();
    let result = notifier.notify_delete("tx_123", &env);
    assert!(result.is_ok());
}


// --- Transaction pool enqueue tests ---

#[test]
fn handle_self_signed_enqueues_to_pool() {
    let (sentinel, registry) = make_sentinel_fixture();

    // Create a self-signed transaction (receiver is empty)
    let tx = Transaction {
        id: "tx_pool_enqueue_signed".into(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: b"alice".to_vec(),
        receiver: vec![],
        amount: Some(100),
        timestamp: 1000,
        result_hash: vec![],
        sender_signature: vec![],
    };

    // Pre-register the transaction (as handle_self_signed in the real flow does)
    registry.register_pending(tx.id.clone()).unwrap();

    // Call handle_self_signed directly — it should enqueue to pool
    sentinel.handle_self_signed(tx.clone(), 100).unwrap();

    // Verify the transaction was enqueued to the pool
    let pool_txs = registry.get_ordered_transactions(&[1], 10).unwrap();
    assert!(!pool_txs.is_empty());
    assert_eq!(pool_txs[0].id, tx.id);
}


// --- AUDIT 5.9: token.is_self_verified drives routing through the pipeline ---

#[test]
fn handle_process_request_routes_self_verified_token_owner_operation() {
    // An owner-operated (self-verified) token whose `owner` == the tx sender,
    // with a *normal* action ("Transfer" — deliberately NOT "SelfSigned"),
    // routes to `handle_self_signed`: no Executor/Finalizer, no Preload, and
    // it lands in the committer's ordered pool. Routing is driven by
    // `token.is_self_verified`, not by `tx.action`.
    let sender_identity = NodeIdentity::generate_in_memory();
    let sender_pk = sender_identity.ed25519.public_key().expect("sender pubkey");

    // Build the token via the real mint path (AUDIT 5.9): owner is recorded as
    // a hex string, is_self_verified = true.
    let token =
        TokenFactory::mint_user_token(sender_pk.clone(), vec![1], "test".into()).expect("token mints");

    let dp = StubDataProvider::new().with_token(vec![1], "token".into(), token);
    let (sentinel, registry) = make_sentinel_fixture_with_data_provider(dp);

    // The owner signs a Process message (sender == owner, envelope sender == sender).
    let mut tx = Transaction {
        id: "tx_self_signed_owner".into(),
        action: "Transfer".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: sender_pk.clone(),
        receiver: vec![],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    c3_sign(&mut tx, &sender_identity);
    let msg = c3_process_message(sender_pk.clone(), tx);

    let result = sentinel.handle_process_request(msg);
    assert!(result.is_ok(), "owner self-signed tx must be accepted, got {result:?}");

    // Accepted, still registered, and admitted to the ordered pool (the
    // self-signed path enqueues it for the committer; the executor-preload
    // step is skipped).
    assert!(
        registry.contains("tx_self_signed_owner"),
        "owner self-signed tx must remain in the registry"
    );
    let pool_txs = registry.get_ordered_transactions(&[1], 10).unwrap();
    assert!(
        pool_txs.iter().any(|t| t.id == "tx_self_signed_owner"),
        "owner self-signed tx must land in the committer's ordered pool"
    );
}


#[test]
fn handle_process_request_rejects_self_verified_tx_from_non_owner() {
    // AUDIT 5.9: a self-verified token requires `sender == owner`. A tx whose
    // sender is NOT the token owner is rejected by the SelfSigned spec's
    // owner-gate (NotTokenOwner) and never admitted to the ordered pool — even
    // though `tx.action` is a standard action and the envelope is correctly
    // signed by the (non-owner) submitter. This is the discriminator: an
    // action-based (or owner-agnostic) route would admit the tx.
    let owner_identity = NodeIdentity::generate_in_memory();
    let owner_pk = owner_identity.ed25519.public_key().expect("owner pubkey");

    let token = TokenFactory::mint_user_token(owner_pk.clone(), vec![1], "test".into())
        .expect("token mints");
    let dp = StubDataProvider::new().with_token(vec![1], "token".into(), token);
    let (sentinel, registry) = make_sentinel_fixture_with_data_provider(dp);

    // A DIFFERENT identity submits the tx (sender != owner).
    let attacker_identity = NodeIdentity::generate_in_memory();
    let attacker_pk = attacker_identity.ed25519.public_key().expect("attacker pubkey");

    let mut tx = Transaction {
        id: "tx_self_signed_non_owner".into(),
        action: "Transfer".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: attacker_pk.clone(), // not the token owner
        receiver: vec![],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    c3_sign(&mut tx, &attacker_identity);
    let msg = c3_process_message(attacker_pk.clone(), tx);

    // The check fails closed in-pipeline (transition_to_failed → Ok), but the
    // tx must NOT be admitted to the pool.
    let result = sentinel.handle_process_request(msg);
    assert!(result.is_ok(), "rejection is handled in-pipeline, got {result:?}");
    assert!(
        !registry
            .get_ordered_transactions(&[1], 10)
            .unwrap()
            .iter()
            .any(|t| t.id == "tx_self_signed_non_owner"),
        "non-owner tx must NOT be admitted to the ordered pool"
    );
}


#[test]
fn handle_process_request_enqueues_standard_tx_to_pool() {
    let (sentinel, registry) = make_sentinel_fixture();
    let (token, _node_registry) = (
        {
            let mut token = Token::new();
            token.set_metadata("owner".to_string(), "bob".to_string());
            token
        },
        make_test_node_registry(),
    );

    // Create a standard transaction (sender != owner, Executed spec)
    let tx = Transaction {
        id: "tx_pool_enqueue_std".into(),
        action: "Process".into(),
        token_id: vec![2],
        bid: None,
        sequence_number: 2,
        sender: b"bob".to_vec(),
        receiver: b"carol".to_vec(),
        amount: Some(50),
        timestamp: 2000,
        result_hash: vec![],
        sender_signature: vec![],
    };

    // Pre-register the transaction (as handle_process_request does)
    let tx_id = tx.id.clone();
    registry.register_pending(tx_id.clone()).unwrap();
    registry.acquire_transaction(&tx_id).unwrap();

    // Manually transition to Validated + enqueue (simulating the new code path
    // that runs in handle_process_request before send_to_executor_for_preload)
    let risk = TransactionRiskFactor {
                    affected_parties: 2,
                    amount: 50,
                    is_contract: false,
                    is_multi_party: false,
                };
    let _ = registry.transition_to_validated_and_enqueue(
        &tx_id,
        tx.clone(),
        TransactionValidationResult {
            is_valid: true,
            risk,
            failure_reasons: vec![],
            finalizer_public_key: vec![3],
        },
    );

    // Verify the transaction was enqueued to the pool for token [2]
    let pool_txs = registry.get_ordered_transactions(&[2], 10).unwrap();
    assert!(!pool_txs.is_empty());
    assert_eq!(pool_txs[0].id, tx_id);
}


#[test]
fn handle_process_request_rejects_duplicate_nonce() {
    // Phase 5.6 / H14: a replayed (token, sender, sequence_number) is rejected via
    // SentinelError::Registry — even when a *different* transaction id carries it.
    //
    // The default test env fails every non-zero-risk transaction at the gas/risk gate
    // (override_quorum_percentage == 0.0, but any 2-party transfer scores > 0), and the
    // plain fixture registers no token. To let `validate_transaction` succeed and reach
    // the pool enqueue (Step 6), we raise the risk gate and register token [1].
    let mut env_data = make_test_env_data();
    env_data.override_quorum_percentage = 100.0;
    let mut token = Token::new();
    token.set_metadata("owner".to_string(), "bob".to_string());
    let data_provider =
        StubDataProvider::new().with_token(vec![1], "token".to_string(), token);
    let (sentinel, registry) =
        make_sentinel_fixture_with_env_and_data_provider(data_provider, env_data);

    // A valid sender signs the canonical transaction bytes (C3 gate passes).
    let sender_identity = NodeIdentity::generate_in_memory();
    let sender_pk = sender_identity.ed25519.public_key().expect("sender public key");

    // First tx: (token [1], sender, seq=5). Valid signature + envelope binding.
    let mut tx1 = Transaction {
        id: "tx_nonce_1".into(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 5,
        sender: sender_pk.clone(),
        receiver: b"carol".to_vec(),
        amount: Some(50),
        timestamp: 1,
        result_hash: vec![],
        sender_signature: vec![],
    };
    c3_sign(&mut tx1, &sender_identity);
    let msg1 = c3_process_message(sender_pk.clone(), tx1);
    // First tx must reach the pool enqueue (Step 6), so the nonce is now consumed.
    let _ = sentinel.handle_process_request(msg1);
    assert!(registry
        .get_ordered_transactions(&[1], 10)
        .unwrap()
        .iter()
        .any(|t| t.id == "tx_nonce_1"));

    // Second tx: a DIFFERENT id carrying the SAME (token [1], sender, seq=5) — a replay.
    let mut tx2 = Transaction {
        id: "tx_nonce_2".into(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 5,
        sender: sender_pk.clone(),
        receiver: b"carol".to_vec(),
        amount: Some(50),
        timestamp: 2,
        result_hash: vec![],
        sender_signature: vec![],
    };
    c3_sign(&mut tx2, &sender_identity);
    let msg2 = c3_process_message(sender_pk.clone(), tx2);
    match sentinel.handle_process_request(msg2) {
        Err(SentinelError::Registry(_)) => {}
        other => panic!("replayed nonce must be rejected via Registry, got {other:?}"),
    }
}


#[test]
fn pool_ordering_is_deterministic() {
    let (sentinel, registry) = make_sentinel_fixture();
    let _ = sentinel;

    // Insert 3 transactions with different sequence numbers and senders.
    // Pool ordering: sender ASC, then sequence_number ASC, then timestamp ASC.
    for i in 0..3 {
        let tx = Transaction {
            id: format!("tx_order_{}", i),
            action: "Process".into(),
            token_id: vec![3],
            bid: None,
            sequence_number: i + 1,
            sender: vec![(i + 1) as u8], // sender 1, 2, 3
            receiver: vec![10],
            amount: Some(10),
            timestamp: 3000,
            result_hash: vec![],
            sender_signature: vec![],
        };
        let tx_id = tx.id.clone();
        registry.register_pending(tx_id.clone()).unwrap();
        let _ = registry.transition_to_validated_and_enqueue(
            &tx_id,
            tx,
            TransactionValidationResult {
                is_valid: true,
                risk: TransactionRiskFactor {
                    affected_parties: 2,
                    amount: 50,
                    is_contract: false,
                    is_multi_party: false,
                },
                failure_reasons: vec![],
                finalizer_public_key: vec![4],
            },
        );
    }

    // Dequeue should return in deterministic order:
    // sender [1], seq 1 → sender [2], seq 2 → sender [3], seq 3
    let ordered = registry.get_ordered_transactions(&[3], 10).unwrap();
    assert_eq!(ordered.len(), 3);
    assert_eq!(ordered[0].id, "tx_order_0"); // sender [1]
    assert_eq!(ordered[1].id, "tx_order_1"); // sender [2]
    assert_eq!(ordered[2].id, "tx_order_2"); // sender [3]
}


// --- Shard-aware routing tests ---

#[test]
fn get_shard_executors_returns_executors() {
    let mut executors = pneumatic_core::epoch::ExecutorSet::default();
    for i in 0..4 {
        executors.executors.insert(vec![i as u8], 100 + i);
    }
    let data_provider = Arc::new(
        StubDataProvider::new().with_executor_set(1, executors)
    );
    let registry = Arc::new(PendingTransactionRegistry::new());
    let node_registry = make_test_node_registry();

    let mut env_data = make_test_env_data();
    env_data.shard_count = 2;

    let gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Sentinel,
        make_test_config(),
        300,
        env_data.asym_crypto_provider.clone(),
    ));
    let sentinel = Sentinel::new(
        make_test_config(),
        Arc::new(env_data),
        node_registry,
        registry.clone(),
        gossiper,
        data_provider,
        Arc::new(SimpleShieldedPoolView::new(10)),
    );

    // Shard 0 and shard 1 should each return some executors
    let shard0 = sentinel.get_shard_executors("tx-0", 1);
    let shard1 = sentinel.get_shard_executors("tx-1", 1);
    assert!(shard0.is_ok() && shard1.is_ok());
    let s0 = shard0.unwrap();
    let s1 = shard1.unwrap();
    assert!(!s0.is_empty() && !s1.is_empty());
    // The two shards should have different members (or at least one)
    let mut s0_sorted = s0;
    let mut s1_sorted = s1;
    s0_sorted.sort();
    s1_sorted.sort();
    assert!(s0_sorted != s1_sorted || s0_sorted.len() + s1_sorted.len() <= 4);
}


#[test]
fn get_shard_executors_changes_with_mined_tip() {
    use pneumatic_core::epoch::ExecutorSet;

    let executors = ExecutorSet {
        executors: [
            (vec![1], 100),
            (vec![2], 100),
            (vec![3], 100),
            (vec![4], 100),
        ]
        .into_iter()
        .collect(),
    };

    let empty_tip = StubDataProvider::new().with_executor_set(1, executors.clone());
    let one_block_tip = StubDataProvider::new()
        .with_executor_set(1, executors)
        .with_token(vec![1], "test".to_string(), token_with_one_block());

    let mut env_data = make_test_env_data();
    env_data.shard_count = 2;

    let (s_empty, _r) =
        make_sentinel_fixture_with_env_and_data_provider(empty_tip, env_data.clone());
    let (s_block, _r) =
        make_sentinel_fixture_with_env_and_data_provider(one_block_tip, env_data);

    let mut shard_empty = Vec::new();
    let mut shard_block = Vec::new();
    for i in 0..50 {
        let tx_id = format!("tx_shard_tip_{i}");
        shard_empty.push(s_empty.get_shard_executors(&tx_id, 1).unwrap());
        shard_block.push(s_block.get_shard_executors(&tx_id, 1).unwrap());
    }

    assert_ne!(
        shard_empty,
        shard_block,
        "executor shard assignment must depend on the mined chain tip"
    );
}


/// True call-site discriminator for the executor path:
/// `send_to_executor_for_preload` routes on `current_epoch`.
#[test]
fn send_to_executor_for_preload_follows_current_epoch() {
    use pneumatic_core::epoch::ExecutorSet;

    // Only epoch 1 has an executor set; epoch 2 is absent.
    let data_provider = StubDataProvider::new().with_executor_set(
        1,
        ExecutorSet {
            executors: [(vec![1], 100), (vec![2], 100)].into_iter().collect(),
        },
    );
    let (sentinel, _registry) =
        make_sentinel_fixture_with_env_and_data_provider(data_provider, make_test_env_data_sharded());

    let tx = Transaction {
        id: "tx_preload_epoch".to_string(),
        action: "Transfer".to_string(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: vec![1],
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };

    // advance_epoch(1) is a no-op at boot; routing stays on epoch 1 → valid set.
    sentinel.advance_epoch(1);
    assert!(sentinel.send_to_executor_for_preload(&tx).is_ok());

    // advance_epoch(2): no executor set for epoch 2 → routing must fail closed.
    // Under the literal-1 bug the handler would keep reading epoch 1 → still Ok.
    sentinel.advance_epoch(2);
    let err = sentinel.send_to_executor_for_preload(&tx).unwrap_err();
    match err {
        SentinelError::Routing(msg) => assert_eq!(msg, "No executor set for epoch 2"),
        other => panic!("expected Routing error, got {:?}", other),
    }
}

