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
                id: "tx_terminal".into(), action: "Transfer".into(),
                token_id: vec![], bid: None, sequence_number: 0,
                sender: vec![], receiver: vec![], amount: None,
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
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

    // Two sentinels: identical stake snapshot + epoch, differing only in the
    // mined tip exposed by the data provider (empty chain vs. one block).
    let empty_tip = StubDataProvider::new().with_stake_snapshot(1, stake.clone());
    let one_block_tip = StubDataProvider::new()
        .with_stake_snapshot(1, stake)
        .with_token(vec![1], "test".to_string(), token_with_one_block());

    let (s_empty, _r) =
        make_sentinel_fixture_with_env_and_data_provider(empty_tip, make_test_env_data());
    let (s_block, _r) =
        make_sentinel_fixture_with_env_and_data_provider(one_block_tip, make_test_env_data());

    let mut finalizers_empty = Vec::new();
    let mut finalizers_block = Vec::new();
    for i in 0..50 {
        let tx_id = format!("tx_finalizer_tip_{i}");
        finalizers_empty
            .push(s_empty.assign_finalizer_deterministic(&tx_id, 1).unwrap());
        finalizers_block
            .push(s_block.assign_finalizer_deterministic(&tx_id, 1).unwrap());
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
        );
    let (sentinel, registry) = make_sentinel_fixture_with_data_provider(data_provider);

    // Put the transaction into Finalizing with finalizer_key = vec![1]
    // (a member of the epoch-1 stake set) — i.e. the assigned finalizer rejects.
    let rejected_key = vec![1];
    make_finalizing_entry(&registry, "tx_reject_epoch", rejected_key.clone());

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
    assert!(sentinel.on_data_received(raw).is_ok());

    // The discriminators: the reassignment must land on the epoch-2 pick, not the
    // epoch-1 pick (which is what the literal-1 bug would produce).
    let epoch2_pick = sentinel
        .assign_finalizer_deterministic_retry("tx_reject_epoch", 2, &rejected_key)
        .unwrap();
    let epoch1_pick = sentinel
        .assign_finalizer_deterministic_retry("tx_reject_epoch", 1, &rejected_key)
        .unwrap();
    assert_ne!(epoch1_pick, epoch2_pick, "the two epochs must pick different finalizers for the test to be meaningful");
    assert!(
        registry.is_requested_finalizer("tx_reject_epoch", &epoch2_pick),
        "entry must be reassigned to the epoch-2 finalizer"
    );
    assert!(
        !registry.is_requested_finalizer("tx_reject_epoch", &epoch1_pick),
        "entry must NOT be reassigned to the epoch-1 finalizer"
    );
}

