//! ADR-004 enforcement at the Finalizer: the three inbound paths refuse a
//! transaction this node was not chosen to finalize.
//!
//! The happy-path fixtures route their transactions here, so on their own they prove
//! nothing about the gate — a permissive enough gate passes them too. What makes the
//! routing decision a constraint is the refusals below: a transaction assigned to
//! someone else must not be registered, must not have its vote counted, and must not
//! reach a block. The last one is the property the multi-host rehearsal measured —
//! one transaction, four candidate blocks, because every Finalizer that received it
//! felt entitled to build one.

use super::helpers::*;
use super::super::*;

/// Build a preload frame for `tx`.
fn preload_frame(tx: &Transaction) -> Message {
    Message {
        chain_id: "test_env".to_string(),
        action: String::from("PreloadForFinalizer"),
        body: serialize_to_bytes_rmp(tx).unwrap(),
        signature: vec![],
        public_key: vec![9, 8, 7],
        stake_set: None,
    }
}

fn routing_fixture() -> (Finalizer, Arc<PendingTransactionRegistry>) {
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry.clone(),
        Arc::new(StubDataProvider::new()),
    );
    (finalizer, pending_registry)
}

/// A transaction the Sentinel routed to a *different* Finalizer, attested by a
/// Sentinel this registry knows. Attested honestly, so the only thing wrong with it
/// is the one thing the gate exists to read.
fn routed_elsewhere(finalizer: &Finalizer, tx_id: &str) -> Transaction {
    let mut tx = make_routed_tx(tx_id);
    let sentinel = NodeIdentity::generate_in_memory();
    register_sentinel(&finalizer.node_registry, &sentinel);
    let other_finalizer = vec![0xDE; 32];
    tx.selection = Some(
        pneumatic_core::transactions::SelectionRecord::attest(
            &sentinel.ed25519,
            tx_id,
            pneumatic_core::epoch::FINALIZER_DOMAIN,
            other_finalizer,
            finalizer.current_epoch,
            vec![0x5A; 32],
        )
        .expect("the sentinel attests its routing decision"),
    );
    tx
}

/// A hand-built transaction with no routing decision.
fn make_routed_tx(tx_id: &str) -> Transaction {
    Transaction {
        payload: vec![],
        gas_limit: 0,
        id: tx_id.to_string(),
        action: "Transfer".to_string(),
        token_id: vec![0, 1, 2],
        bid: None,
        sequence_number: 1,
        sender: vec![10, 20, 30],
        receiver: vec![40, 50, 60],
        amount: Some(50),
        timestamp: 1000,
        result_hash: vec![],
        sender_signature: vec![],
        result_data: vec![],
        selection: None,
    }
}

#[tokio::test]
async fn preload_for_a_transaction_assigned_elsewhere_is_refused_and_stores_nothing() {
    let (finalizer, pending_registry) = routing_fixture();
    let tx = routed_elsewhere(&finalizer, "not_mine_tx");

    let err = finalizer
        .handle_preload(&preload_frame(&tx))
        .await
        .expect_err("a transaction assigned to another Finalizer is not this node's to hold");
    assert!(
        matches!(err, PneumaticError::Routing(_)),
        "expected a Routing refusal, got {err:?}"
    );
    assert!(
        pending_registry.get_transaction_mut("not_mine_tx").is_err(),
        "refusing must not leave an entry behind — every downstream path reads this \
         registry, so a stored not-mine transaction is a future block"
    );
}

#[tokio::test]
async fn preload_with_no_routing_decision_at_all_is_refused() {
    // `None` is not "unconstrained". A transaction that reaches a Finalizer without a
    // decision arrived through something other than the routed path, and the only
    // reading that does not grant a block to whoever asks first is to refuse it.
    let (finalizer, _registry) = routing_fixture();
    let tx = make_routed_tx("no_decision_tx");

    let err = finalizer
        .handle_preload(&preload_frame(&tx))
        .await
        .expect_err("no decision is a refusal, not a free-for-all");
    assert!(
        matches!(err, PneumaticError::Routing(_)),
        "expected a Routing refusal, got {err:?}"
    );
}

#[tokio::test]
async fn an_attestation_from_a_key_that_is_not_a_sentinel_is_refused() {
    // The gate that signature verification alone cannot provide. This record is
    // correctly signed and verifies against its own attester key — a verifier that
    // only checked the signature would accept it, and so would a Finalizer that let
    // itself be assigned by anyone holding any keypair. The attester has to resolve to
    // a registered Sentinel.
    let (finalizer, _registry) = routing_fixture();
    let mut tx = make_routed_tx("self_asserted_tx");
    let impostor = NodeIdentity::generate_in_memory(); // deliberately NOT registered
    tx.selection = Some(
        pneumatic_core::transactions::SelectionRecord::attest(
            &impostor.ed25519,
            "self_asserted_tx",
            pneumatic_core::epoch::FINALIZER_DOMAIN,
            finalizer.public_key.clone(),
            finalizer.current_epoch,
            vec![0x5A; 32],
        )
        .expect("the impostor signs a decision naming this finalizer"),
    );

    // Prove the signature is not what fails here. `verify` checks the record against
    // the key the record itself names as its attester, which is exactly the check a
    // signature-only verifier performs — and it passes. The refusal below therefore
    // comes from the role lookup, not from the cryptography.
    assert!(
        tx.selection
            .as_ref()
            .unwrap()
            .verify(
                "self_asserted_tx",
                pneumatic_core::epoch::FINALIZER_DOMAIN,
                &pneumatic_core::crypto::Ed25519Provider::generate()
            )
            .expect("the record is well-formed"),
        "the premise of this test is a record whose signature verifies; if that stops          being true the test proves nothing"
    );

    let err = finalizer
        .handle_preload(&preload_frame(&tx))
        .await
        .expect_err("a self-asserted decision is not a routing decision");
    assert!(
        matches!(err, PneumaticError::Routing(_)),
        "expected a Routing refusal, got {err:?}"
    );
}

#[tokio::test]
async fn a_vote_for_a_transaction_assigned_elsewhere_is_not_counted() {
    // The entry exists (this registry is shared on a composite, or the preload
    // predates the gate), and it names someone else. The vote must not enter the
    // collector, because a collected vote is a step toward a signature over a block.
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry.clone(),
        Arc::new(StubDataProvider::new()),
    );

    // Put the transaction in the registry with a decision naming another Finalizer.
    let tx = routed_elsewhere(&finalizer, "test_tx_001");
    {
        let mut entry = pending_registry
            .get_transaction_mut("test_tx_001")
            .expect("the fixture entry exists");
        entry.state = TransactionState::Preloaded { transaction: tx };
    }

    let message = build_signed_sign_message("test_env", b"test_tx_001", vec![1, 2, 3], 10, &voter);
    let err = finalizer
        .handle_signature(&message)
        .await
        .expect_err("a vote for another Finalizer's transaction is refused");
    assert!(
        matches!(err, PneumaticError::Routing(_)),
        "expected a Routing refusal, got {err:?}"
    );
    assert_eq!(
        finalizer.signature_count("test_tx_001"),
        0,
        "the collector must not credit a Finalizer that does not own the transaction"
    );
}

#[tokio::test]
async fn the_finalize_choke_point_refuses_a_transaction_assigned_elsewhere() {
    // `try_finalize_optimistic` carries the same gate as the two handlers, and it is
    // the one that matters most: it is the last check before a block is signed, so a
    // future caller cannot reach a block by skipping a handler. Reached here directly,
    // with an entry that is otherwise perfectly finalizable.
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let voter_pk = voter.ed25519.public_key().expect("voter pk");
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        pending_registry.clone(),
        Arc::new(StubDataProvider::new()),
    );

    let tx = routed_elsewhere(&finalizer, "test_tx_001");
    {
        let mut entry = pending_registry
            .get_transaction_mut("test_tx_001")
            .expect("the fixture entry exists");
        entry.state = TransactionState::Preloaded { transaction: tx };
    }

    let sig = pneumatic_core::transactions::TransactionSignature {
        transaction_id: b"test_tx_001".to_vec(),
        env_id: vec![],
        transaction_hash: vec![1, 2, 3],
        signature: vec![],
        current_stake: 10,
    };
    let err = finalizer
        .try_finalize_optimistic("test_tx_001", &sig, &voter_pk)
        .await
        .expect_err("the choke point refuses a not-mine transaction");
    assert!(
        matches!(err, PneumaticError::Routing(_)),
        "expected a Routing refusal, got {err:?}"
    );
}
