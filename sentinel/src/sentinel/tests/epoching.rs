//! Epoching tests for the Sentinel, moved from `crate::sentinel::tests`.

use super::helpers::*;
use super::super::*;


#[test]
fn advance_epoch_invalidates_caches() {
    use pneumatic_core::epoch::StakeSet;

    let data_provider = StubDataProvider::new();
    let (mut sentinel, _registry) = make_sentinel_fixture_with_data_provider(data_provider);

    // Initially at epoch 0
    sentinel.advance_epoch(1);
    assert_eq!(*sentinel.current_epoch.lock(), 1);

    // Invalidate and verify caches are cleared
    let snapshot = StakeSet {
        stakers: [(vec![1], 100)].into_iter().collect(),
    };
    sentinel.stake_snapshot_cache.put(1, snapshot);
    assert_eq!(sentinel.stake_snapshot_cache.cached_count(), 1);
    sentinel.advance_epoch(2);
    assert_eq!(sentinel.stake_snapshot_cache.cached_count(), 0);
    assert_eq!(*sentinel.current_epoch.lock(), 2);
}


// -----------------------------------------------------------------------
// Phase 4.2 (AUDIT H5): sentinel routes on the tracked epoch, not a
//                    hardcoded literal `1`. Each test is a discriminator
//                    that fails under the literal-1 bug and passes with the
//                    current_epoch wiring.
// -----------------------------------------------------------------------

/// The core discriminator. After `advance_epoch`, both the executor-shard and
/// the finalizer-stake routing primitives select against the *new* epoch.
#[test]
fn advance_epoch_routes_follows_new_epoch() {
    use pneumatic_core::epoch::{ExecutorSet, StakeSet};

    // Disjoint executor sets and stake snapshots per epoch: any selection from
    // epoch 1 is provably distinct from any selection from epoch 2.
    let data_provider = StubDataProvider::new()
        .with_executor_set(
            1,
            ExecutorSet {
                executors: [(vec![1], 100), (vec![2], 100)].into_iter().collect(),
            },
        )
        .with_executor_set(
            2,
            ExecutorSet {
                executors: [(vec![10], 100), (vec![20], 100)].into_iter().collect(),
            },
        )
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
    let (sentinel, _registry) =
        make_sentinel_fixture_with_env_and_data_provider(data_provider, make_test_env_data_sharded());

    let tx_id = "tx_epoch_route".to_string();

    // advance_epoch(1) is a no-op at boot (init is 1); routing stays on epoch 1.
    sentinel.advance_epoch(1);
    assert_eq!(*sentinel.current_epoch.lock(), 1);
    let exec1 = sentinel
        .get_shard_executors(&tx_id, *sentinel.current_epoch.lock())
        .unwrap();
    let finalizer1 = sentinel
        .assign_finalizer_deterministic(&tx_id, *sentinel.current_epoch.lock())
        .unwrap();

    // advance_epoch(2) moves routing to epoch 2.
    sentinel.advance_epoch(2);
    assert_eq!(*sentinel.current_epoch.lock(), 2);
    let exec2 = sentinel
        .get_shard_executors(&tx_id, *sentinel.current_epoch.lock())
        .unwrap();
    let finalizer2 = sentinel
        .assign_finalizer_deterministic(&tx_id, *sentinel.current_epoch.lock())
        .unwrap();

    // Per-epoch routing must select different targets. Under the literal-1 bug
    // both calls would read epoch 1 → identical selections.
    let mut e1 = exec1.clone();
    e1.sort();
    let mut e2 = exec2.clone();
    e2.sort();
    assert_ne!(e1, e2, "executor selection must change after an epoch advance");
    assert_ne!(finalizer1, finalizer2, "finalizer selection must change after an epoch advance");

    // And each pick must come from its own epoch's disjoint key set.
    assert!(
        [vec![1], vec![2], vec![3]].contains(&finalizer1)
            && [vec![10], vec![20], vec![30]].contains(&finalizer2),
        "each finalizer pick must be drawn from its own epoch's stake snapshot"
    );
}


/// Wiring discriminator: the `BlockFinalized` action now advances the epoch
/// (and only a registered finalizer may do so — see fail-closed test).
#[test]
fn block_finalized_advances_epoch() {
    use pneumatic_core::blocks::Block;
    use pneumatic_core::conns::Connection;

    // Minimal no-op connection so register_peer accepts a peer.
    struct NoOpConnection;
    #[async_trait::async_trait]
    impl Connection for NoOpConnection {
        async fn send(
            &self,
            _data: &Vec<u8>,
        ) -> Result<(), pneumatic_core::conns::ConnError> {
            Ok(())
        }
    }

    // Register a finalizer peer so the role guard recognizes the sender.
    let (sentinel, _registry) = make_sentinel_fixture();
    let finalizer_key = vec![0xA3; 32];
    assert!(sentinel.node_registry.register_peer(
        finalizer_key.clone(),
        [3u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(NoOpConnection),
    ));

    // Build a finalized block bound to epoch 3.
    let block = Block {
        signed_trans: pneumatic_core::transactions::SignedTransaction::test_transaction(),
        token_metadata: std::collections::HashMap::new(),
        previous_hash: vec![1, 2, 3],
        current_hash: vec![4, 5, 6],
        timestamp: 0,
        finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 3,
    };
    let body = serialize_to_bytes_rmp(&block).unwrap();

    // Message from the registered finalizer: message.public_key is the sender.
    let msg = Message {
        chain_id: "test".into(),
        action: "BlockFinalized".into(),
        body,
        signature: vec![],
        public_key: finalizer_key.clone(),
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();

    assert!(sentinel.on_data_received(raw).is_ok());
    assert_eq!(*sentinel.current_epoch.lock(), 3);
}


/// Fail-closed guardrails for the epoch-advance handler: only a registered
/// finalizer may advance, and the advance is monotonic (never rewind).
#[test]
fn block_finalized_fail_closed() {
    use pneumatic_core::blocks::Block;
    use pneumatic_core::conns::Connection;

    struct NoOpConnection;
    #[async_trait::async_trait]
    impl Connection for NoOpConnection {
        async fn send(
            &self,
            _data: &Vec<u8>,
        ) -> Result<(), pneumatic_core::conns::ConnError> {
            Ok(())
        }
    }

    fn make_block(epoch: u64) -> Block {
        Block {
            signed_trans: pneumatic_core::transactions::SignedTransaction::test_transaction(),
            token_metadata: std::collections::HashMap::new(),
            previous_hash: vec![],
            current_hash: vec![],
            timestamp: 0,
            finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: epoch,
        }
    }

    let (sentinel, _registry) = make_sentinel_fixture();
    let finalizer_key = vec![0xB1; 32];
    assert!(sentinel.node_registry.register_peer(
        finalizer_key.clone(),
        [1u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(NoOpConnection),
    ));

    // (b) A non-finalizer sender is rejected; the epoch must not move.
    // Under no role guard an attacker could advance the epoch to an arbitrary
    // (stale/empty) executor set — an availability risk.
    let attacker_body = serialize_to_bytes_rmp(&make_block(5)).unwrap();
    let attacker_msg = Message {
        chain_id: "test".into(),
        action: "BlockFinalized".into(),
        body: attacker_body,
        signature: vec![],
        public_key: vec![0xDE; 32], // never registered as a finalizer
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&attacker_msg).unwrap();
    match sentinel.on_data_received(raw) {
        Err(SentinelError::Registry(msg)) => assert!(msg.contains("non-finalizer")),
        other => panic!("expected Registry error for non-finalizer, got {:?}", other),
    }
    assert_eq!(*sentinel.current_epoch.lock(), 1);

    // (c) A malformed body is rejected (encoding failure).
    let bad_body = serialize_to_bytes_rmp(&"this is not a block".to_string()).unwrap();
    let bad_msg = Message {
        chain_id: "test".into(),
        action: "BlockFinalized".into(),
        body: bad_body,
        signature: vec![],
        public_key: finalizer_key.clone(),
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&bad_msg).unwrap();
    match sentinel.on_data_received(raw) {
        Err(SentinelError::Encoding(_)) => {}
        other => panic!("expected Encoding error for malformed body, got {:?}", other),
    }
    assert_eq!(*sentinel.current_epoch.lock(), 1);

    // (a) A stale/replayed block (epoch <= current) must not rewind the epoch.
    // Advance to epoch 2, then feed a block bound to epoch 1. Under no monotonic
    // guard the epoch would roll back to 1; the guard keeps it at 2.
    sentinel.advance_epoch(2);
    let stale_body = serialize_to_bytes_rmp(&make_block(1)).unwrap();
    let stale_msg = Message {
        chain_id: "test".into(),
        action: "BlockFinalized".into(),
        body: stale_body,
        signature: vec![],
        public_key: finalizer_key.clone(),
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&stale_msg).unwrap();
    assert!(sentinel.on_data_received(raw).is_ok(), "stale block still routes to the handler");
    assert_eq!(
        *sentinel.current_epoch.lock(), 2,
        "stale block must not rewind the epoch"
    );
}

// -----------------------------------------------------------------------
// Phase S5.1 — the shielded transfer handler (the real path; the S3.2
// stub is retired).
//
// The stub spec (plan, Decision 6) pins the handler's wiring without
// paying halo2 keygen: its registration name is the real
// `ShieldedValidationSpec::NAME` constant (the seam the handler looks up,
// Decision 3) and its `validate_shielded` is cheap — empty proof ⇒
// `InvalidShieldedProof`, else `valid` with zero risk. The real
// `ShieldedValidationSpec`'s cryptographic behavior is pinned in the core
// suite (S4.1.5); it appears here only in the two stale-state tests and
// the `#[ignore]`d live end-to-end, each built to fail at checks 2/3
// before check 4's lazy verifier (no keygen in the default run).
// -----------------------------------------------------------------------

use pneumatic_core::conns::Connection;
use pneumatic_core::crypto::Ed25519Provider;
use pneumatic_core::epoch::StakeSet;
use group::GroupEncoding;
use pasta_curves::pallas::{Base as Fp, Scalar as Fq};
use ff::PrimeField; // `to_repr()` in the stale-root test

/// A `Connection` that records every "sent" payload byte-verbatim (same
/// shape as the transaction_notifier test helpers).
struct RecordingConnection {
    recorder: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl Connection for RecordingConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), ConnError> {
        self.recorder.lock().unwrap().push(data.clone());
        Ok(())
    }
}

