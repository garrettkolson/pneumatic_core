//! Finalizing tests for the Sentinel, moved from `crate::sentinel::tests`.

use super::helpers::*;
use super::super::*;


#[test]
fn handle_confirmation_valid_finalizer_transitions_to_committed() {
    let (sentinel, registry) = make_sentinel_fixture();
    let finalizer_key = vec![99];
    make_finalizing_entry(&registry, "tx_confirm", finalizer_key.clone());

    // Send a "Confirm" message from the assigned finalizer.
    let msg = Message {
        chain_id: "test".into(),
        action: "Confirm".into(),
        body: serialize_to_bytes_rmp(&"tx_confirm".to_string()).unwrap(),
        signature: vec![],
        public_key: finalizer_key,
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_ok());

    // Transaction should now be in Committed state.
    let entry = registry.get_transaction_mut("tx_confirm").unwrap();
    assert!(matches!(entry.state, TransactionState::Committed { .. }));
}


#[test]
fn handle_confirmation_unassigned_finalizer_returns_error() {
    let (sentinel, registry) = make_sentinel_fixture();
    let finalizer_key = vec![99];
    make_finalizing_entry(&registry, "tx_bad_confirm", finalizer_key.clone());

    // Send a "Confirm" message from an unassigned finalizer.
    let msg = Message {
        chain_id: "test".into(),
        action: "Confirm".into(),
        body: serialize_to_bytes_rmp(&"tx_bad_confirm".to_string()).unwrap(),
        signature: vec![],
        public_key: vec![1, 2, 3], // wrong key
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_err());
    match result.unwrap_err() {
        SentinelError::Registry(msg) => assert!(msg.contains("unassigned")),
        _ => panic!("expected Registry error"),
    }
}


#[test]
fn handle_confirmation_not_in_finalizing_state_returns_error() {
    let (sentinel, registry) = make_sentinel_fixture();
    // Register but don't set a finalizer — stays in Pending.
    registry.register_pending("tx_no_finalizer".into()).unwrap();

    let msg = Message {
        chain_id: "test".into(),
        action: "Confirm".into(),
        body: serialize_to_bytes_rmp(&"tx_no_finalizer".to_string()).unwrap(),
        signature: vec![],
        public_key: vec![99],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_err());
}


// --- handle_rejection tests ---

#[test]
fn handle_rejection_reassigns_to_new_finalizer() {
    let (sentinel, registry) = make_sentinel_fixture();
    let rejected_key = vec![1];
    let new_key = vec![2];
    make_finalizing_entry(&registry, "tx_reject", rejected_key.clone());

    // Send a "Reject" message from the assigned finalizer.
    let msg = Message {
        chain_id: "test".into(),
        action: "Reject".into(),
        body: serialize_to_bytes_rmp(&"tx_reject".to_string()).unwrap(),
        signature: vec![],
        public_key: rejected_key,
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_err());
    match result.unwrap_err() {
        SentinelError::Registry(msg) => assert!(msg.contains("No alternative finalizer")),
        e => panic!("expected Registry error, got {:?}", e),
    }
}


#[test]
fn handle_rejection_unassigned_finalizer_returns_error() {
    let (sentinel, registry) = make_sentinel_fixture();
    let assigned_key = vec![99];
    make_finalizing_entry(&registry, "tx_bad_reject", assigned_key.clone());

    // Send a "Reject" message from a non-assigned finalizer.
    let msg = Message {
        chain_id: "test".into(),
        action: "Reject".into(),
        body: serialize_to_bytes_rmp(&"tx_bad_reject".to_string()).unwrap(),
        signature: vec![],
        public_key: vec![5, 6, 7],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_err());
    match result.unwrap_err() {
        SentinelError::Registry(msg) => assert!(msg.contains("non-assigned")),
        _ => panic!("expected Registry error"),
    }
}


#[test]
fn handle_rejection_terminal_state_returns_error() {
    let (sentinel, registry) = make_sentinel_fixture();
    // Register a pending transaction (terminal state = Failed/Committed would reject acquire)
    registry.register_pending("tx_terminal".into()).unwrap();
    // Transition to Failed (terminal)
    if let Ok(mut entry) = registry.get_transaction_mut("tx_terminal") {
        entry.transition_to_failed(
            Transaction {
                payload: vec![], gas_limit: 0,
                id: "tx_terminal".into(), action: "Transfer".into(),
                token_id: vec![], bid: None, sequence_number: 0,
                sender: vec![], receiver: vec![], amount: None,
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
            
            result_data: vec![],selection: None,
            },
            vec![],
        );
    }

    let msg = Message {
        chain_id: "test".into(),
        action: "Reject".into(),
        body: serialize_to_bytes_rmp(&"tx_terminal".to_string()).unwrap(),
        signature: vec![],
        public_key: vec![1],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_err());
    match result.unwrap_err() {
        SentinelError::TransactionInTerminalState(id) => assert_eq!(id, "tx_terminal"),
        _ => panic!("expected TransactionInTerminalState error"),
    }
}


#[test]
fn assign_finalizer_changes_with_mined_tip() {
    use pneumatic_core::epoch::StakeSet;

    // Spread 3-staker set so finalizer assignments are well distributed.
    let stake = StakeSet {
        stakers: [(vec![10], 10), (vec![20], 30), (vec![30], 60)].into_iter().collect(),
    };

    // Two sentinels: identical stake snapshot + epoch. The mined tip is now the
    // caller's input (see below), so the providers are deliberately identical — if
    // this test still discriminates, it is discriminating on the salt and not on
    // whatever the provider happens to guess.
    let empty_tip = StubDataProvider::new().with_stake_snapshot(1, stake.clone());
    let one_block_tip = StubDataProvider::new().with_stake_snapshot(1, stake);

    let (s_empty, _r) =
        make_sentinel_fixture_with_env_and_data_provider(empty_tip, make_test_env_data());
    let (s_block, _r) =
        make_sentinel_fixture_with_env_and_data_provider(one_block_tip, make_test_env_data());
    // Candidates are staked keys the sentinel can address, so both sentinels need
    // their three stakers registered as Finalizers.
    for s in [&s_empty, &s_block] {
        register_finalizers(s, &[vec![10], vec![20], vec![30]]);
    }

    let salt_empty: Vec<u8> = Vec::new();
    let salt_block = token_with_one_block().blockchain.get_current_chain_state().last_hash_in;
    assert!(!salt_block.is_empty(), "fixture must carry a mined tip for this to discriminate");

    let mut finalizers_empty = Vec::new();
    let mut finalizers_block = Vec::new();
    for i in 0..50 {
        let tx_id = format!("tx_finalizer_tip_{i}");
        finalizers_empty
            .push(s_empty.assign_finalizer_deterministic(&tx_id, 1, &salt_empty).unwrap());
        finalizers_block
            .push(s_block.assign_finalizer_deterministic(&tx_id, 1, &salt_block).unwrap());
    }

    assert_ne!(
        finalizers_empty,
        finalizers_block,
        "finalizer assignments must depend on the mined chain tip"
    );
}


/// True call-site discriminator for the finalizer path:
/// `handle_rejection` reassigns against `current_epoch`'s stake snapshot.
#[test]
fn handle_rejection_follows_current_epoch() {
    use pneumatic_core::epoch::StakeSet;

    // Disjoint stake snapshots per epoch.
    let data_provider = StubDataProvider::new()
        .with_stake_snapshot(
            1,
            StakeSet {
                stakers: [(vec![1], 100), (vec![2], 100), (vec![3], 100)].into_iter().collect(),
            },
        )
        .with_stake_snapshot(
            2,
            StakeSet {
                stakers: [(vec![10], 100), (vec![20], 100), (vec![30], 100)].into_iter().collect(),
            },
        )
        // ADR-019: the reassignment salt comes from the tip of the transaction's own
        // chain, and an unreadable token never yields a salt. This test asserts the
        // reassignment lands on the epoch-2 DETERMINISTIC pick, so it has to supply
        // the token the fixture entry names (token_id = [1]).
        .with_token(
            vec![1],
            make_test_env_data().token_partition_id.clone(),
            token_with_one_block(),
        );
    let (sentinel, registry) = make_sentinel_fixture_with_data_provider(data_provider);
    // The stakers named by both epochs' snapshots are reachable Finalizers.
    register_finalizers(&sentinel, &[vec![1], vec![2], vec![3], vec![10], vec![20], vec![30]]);

    // Put the transaction into Finalizing with finalizer_key = vec![1]
    // (a member of the epoch-1 stake set) — i.e. the assigned finalizer rejects.
    let rejected_key = vec![1];
    make_finalizing_entry(&registry, "tx_reject_epoch", rejected_key.clone());

    // ADR-019: the reassignment salt is the tip of the transaction's own chain.
    // The expected picks are computed BEFORE the rejection is dispatched, so the
    // peer the handler is about to choose can be registered first. Passing the
    // salt explicitly also makes this test discriminate on salt provenance: a
    // handler salting with anything else would miss the expected pick entirely.
    let chain_tip = token_with_one_block().blockchain.get_current_chain_state().last_hash_in;
    assert!(!chain_tip.is_empty(), "fixture must carry a mined tip");
    let epoch2_pick = sentinel
        .assign_finalizer_deterministic_retry("tx_reject_epoch", 2, &rejected_key, &chain_tip)
        .unwrap();
    let epoch1_pick = sentinel
        .assign_finalizer_deterministic_retry("tx_reject_epoch", 1, &rejected_key, &chain_tip)
        .unwrap();
    assert_ne!(epoch1_pick, epoch2_pick, "the two epochs must pick different finalizers for the test to be meaningful");

    // A reassignment that reaches nobody is a reported failure now, so the peer the
    // handler will choose has to be in the bucket for real. `register_peer` is
    // capacity-capped and reports refusal through its return value — ignoring that
    // is how a "the peer is registered" fixture quietly stops being true.
    assert!(
        sentinel.node_registry.register_peer(
            epoch2_pick.clone(),
            [2u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(pneumatic_core::node::registry::NullConnection),
        ),
        "the epoch-2 finalizer must be registerable, or no reassignment can be delivered"
    );

    // Advance to epoch 2, then drive the rejection.
    sentinel.advance_epoch(2);
    let msg = Message {
        chain_id: "test".into(),
        action: "Reject".into(),
        body: serialize_to_bytes_rmp(&"tx_reject_epoch".to_string()).unwrap(),
        signature: vec![],
        public_key: rejected_key.clone(),
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    if let Err(e) = sentinel.on_data_received(raw) {
        panic!("rejection handling must complete, got {:?}", e);
    }

    // The discriminators: the reassignment must land on the epoch-2 pick, not the
    // epoch-1 pick (which is what the literal-1 bug would produce).
    assert!(
        registry.is_requested_finalizer("tx_reject_epoch", &epoch2_pick),
        "entry must be reassigned to the epoch-2 finalizer"
    );
    assert!(
        !registry.is_requested_finalizer("tx_reject_epoch", &epoch1_pick),
        "entry must NOT be reassigned to the epoch-1 finalizer"
    );
}


/// A connection that records what it was handed, so a test can assert the new
/// finalizer actually *receives* the reassigned transaction instead of merely
/// being named in local state.
#[derive(Clone, Default)]
struct RecordingConn {
    sent: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl pneumatic_core::conns::Connection for RecordingConn {
    async fn send(
        &self,
        data: &Vec<u8>,
    ) -> Result<(), pneumatic_core::conns::ConnError> {
        self.sent.lock().unwrap().push(data.clone());
        Ok(())
    }
}

/// Regression for what Phase 0 item 2 uncovered. The handler read the entry a
/// second time with `get_transaction` while it already held that entry's write
/// lock — which cannot succeed — and the `if let Ok(tx)` around it swallowed the
/// error. So the reassignment send **never ran**: a rejected transaction was
/// reassigned in local state and delivered to nobody, with nothing logged and
/// `Ok(())` returned. Nothing recovers it either, because the new finalizer never
/// learns the transaction exists, so no confirmation and no rejection ever comes.
///
/// The payload assertion is the point of this test. "The handler returned Ok" and
/// "the entry names the new finalizer" were both true while the send was dead.
#[test]
fn handle_rejection_delivers_the_transaction_to_the_new_finalizer() {
    use pneumatic_core::epoch::StakeSet;
    use std::sync::{Arc, Mutex};

    let data_provider = StubDataProvider::new()
        .with_stake_snapshot(
            2,
            StakeSet {
                stakers: [(vec![10], 100), (vec![20], 100), (vec![30], 100)]
                    .into_iter()
                    .collect(),
            },
        )
        .with_token(
            vec![1],
            make_test_env_data().token_partition_id.clone(),
            token_with_one_block(),
        );
    let (sentinel, registry) = make_sentinel_fixture_with_data_provider(data_provider);
    // The stakers named by both epochs' snapshots are reachable Finalizers.
    register_finalizers(&sentinel, &[vec![1], vec![2], vec![3], vec![10], vec![20], vec![30]]);

    let rejected_key = vec![1];
    make_finalizing_entry(&registry, "tx_reject_deliver", rejected_key.clone());

    let chain_tip = token_with_one_block().blockchain.get_current_chain_state().last_hash_in;
    let new_key = sentinel
        .assign_finalizer_deterministic_retry("tx_reject_deliver", 2, &rejected_key, &chain_tip)
        .unwrap();

    let recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(
        sentinel.node_registry.register_peer(
            new_key.clone(),
            [3u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConn { sent: Arc::clone(&recorder) }),
        ),
        "the chosen finalizer must be registered for the delivery to be observable"
    );

    sentinel.advance_epoch(2);
    let msg = Message {
        chain_id: "test".into(),
        action: "Reject".into(),
        body: serialize_to_bytes_rmp(&"tx_reject_deliver".to_string()).unwrap(),
        signature: vec![],
        public_key: rejected_key,
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    if let Err(e) = sentinel.on_data_received(raw) {
        panic!("rejection handling must complete, got {:?}", e);
    }

    // The targeted send is synchronous, so there is nothing to poll here: by the
    // time the handler returns, the payload has landed or it never will.
    let sent = recorder.lock().unwrap().clone();
    assert_eq!(
        sent.len(),
        1,
        "the new finalizer must receive exactly one message (zero means the send is dead)"
    );

    let delivered: Message =
        deserialize_rmp_to(&sent[0]).expect("payload must be a Message");
    // The action the Finalizer actually admits. This asserted `"FinalizerRequest"`,
    // a name that appears in no admit list in the workspace: the composite dispatcher
    // resolved it to `UnknownAction` and refused it, so the recovery path this test
    // exists to protect could never deliver on a composite host.
    assert_eq!(delivered.action, "PreloadForFinalizer");
    let delivered_tx: Transaction =
        deserialize_rmp_to(&delivered.body).expect("body must carry the transaction");
    assert_eq!(
        delivered_tx.id, "tx_reject_deliver",
        "the transaction handed to the new finalizer must be the reassigned one"
    );

    // ADR-004: the reassignment must travel *with* the decision it reassigns. The
    // new Finalizer refuses a transaction whose routing decision names someone else,
    // so a re-stamped record is not a nicety — without it the reassignment lands and
    // is declined, which is the same dead end the old "no delivery at all" bug was,
    // just one step later.
    let record = delivered_tx
        .selection
        .as_ref()
        .expect("a reassigned transaction must carry a routing decision for its new owner");
    assert!(
        record.names(&new_key),
        "the record must name the NEW finalizer, not the one that rejected it"
    );
    assert!(
        record.verify(
            &delivered_tx.id,
            pneumatic_core::epoch::FINALIZER_DOMAIN,
            &pneumatic_core::crypto::Ed25519Provider::generate()
        )
        .unwrap_or(false),
        "the re-attestation must verify under the key the record itself names"
    );
    assert!(
        !record.salt.is_empty(),
        "the retry carries the salt the original selection used, so the retry's \
         tie-break is the only thing that moved"
    );
}
