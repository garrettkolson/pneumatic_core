use super::helpers::*;
use super::super::*;
use std::sync::{Arc, Mutex};


#[tokio::test]
async fn propose_blocks_returns_empty_when_not_leader() {
    let (committer, _registry, _dp) = make_committer_for_leader_test(
        vec![99], // committer key
        b"leader".to_vec(), // leader key — different
    );

    let result = committer.propose_blocks(&[1], 10).await.unwrap();
    assert!(result.is_empty());
}


#[tokio::test]
async fn propose_blocks_returns_batch_when_leader_with_pool_items() {
    let (committer, registry, _dp) = make_committer_for_leader_test(
        b"leader".to_vec(), // committer key
        b"leader".to_vec(), // leader key — same, so this IS the leader
    );

    // Bootstrap token so it's in the cache
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);

    // Add a transaction to the pool
    let tx_id = "tx_propose_1".to_string();
    registry.register_pending(tx_id.clone()).unwrap();
    let tx = Transaction {
        id: tx_id.clone(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: b"sender".to_vec(),
        receiver: b"receiver".to_vec(),
        amount: Some(50),
        timestamp: 5000,
        result_hash: vec![],
        sender_signature: vec![],
    };
    registry.transition_to_validated_and_enqueue(
        &tx_id,
        tx.clone(),
        TransactionValidationResult {
            is_valid: true,
            risk: TransactionRiskFactor {
                affected_parties: 2,
                amount: 50,
                is_contract: false,
                is_multi_party: false,
            },
            failure_reasons: vec![],
            finalizer_public_key: vec![5],
        },
    ).unwrap();

    // This committer IS the leader and has pool items — should return a batch
    let result = committer.propose_blocks(&[1], 10).await.unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].trans_id, tx_id.into_bytes());
}


#[tokio::test]
async fn run_epoch_loop_commits_leader_proposed_block() {
    // AUDIT Phase 4.1 / H4 (4.1c): run_epoch_loop must consume propose_blocks output and
    // commit each leader-proposed block — not discard it. A committer that is the leader and
    // has a Validated tx in the pool must grow the chain. Pre-fix (`let _ = self.propose_blocks(...)`)
    // discarded every proposed commit, so the chain never grew; this asserts it does.
    let (committer, registry, _dp) = make_committer_for_leader_test(
        b"leader".to_vec(), // committer key
        b"leader".to_vec(), // leader key — same, so this IS the leader
    );

    // Bootstrap token + genesis chain so commit_block has a validated chain to append to.
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // Add a Validated tx to the pool — this is what propose_blocks dequeues for the leader.
    let tx_id = "tx_epoch_commit".to_string();
    registry.register_pending(tx_id.clone()).unwrap();
    registry
        .transition_to_validated_and_enqueue(
            &tx_id,
            Transaction {
                id: tx_id.clone(),
                action: "Process".into(),
                token_id: vec![1],
                bid: None,
                sequence_number: 1,
                sender: b"alice".to_vec(),
                receiver: b"bob".to_vec(),
                amount: Some(100),
                timestamp: 0,
                result_hash: vec![],
                sender_signature: vec![],
            },
            TransactionValidationResult {
                is_valid: true,
                risk: TransactionRiskFactor {
                    affected_parties: 2,
                    amount: 100,
                    is_contract: false,
                    is_multi_party: false,
                },
                failure_reasons: vec![],
                finalizer_public_key: vec![5],
            },
        )
        .unwrap();

    let before = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();

    let result = committer.run_epoch_loop().await;
    assert!(result.is_ok(), "run_epoch_loop failed: {:?}", result.err());

    // The leader-proposed commit must have been consumed and committed — the chain grew.
    let after = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();
    assert!(
        after >= before + 1,
        "run_epoch_loop should have committed a leader block ({} -> {})",
        before,
        after
    );
}



#[tokio::test]
async fn advance_epoch_bumps_number() {
    let (committer, _registry, _dp) = make_committer_for_leader_test(
        b"leader".to_vec(),
        b"leader".to_vec(),
    );

    // Add a staker so leader selection produces a non-empty result
    committer.stake_store.add_staker(b"leader".to_vec(), 100);

    let before = committer.current_epoch_number.load(Ordering::SeqCst);
    let new_leader = committer.advance_epoch().unwrap();

    let after = committer.current_epoch_number.load(Ordering::SeqCst);
    assert_eq!(after, before + 1);
    assert!(!new_leader.unwrap().is_empty());
}


#[tokio::test]
async fn advance_epoch_leader_changes_with_mined_tip() {
    // AUDIT Phase 5.3 / H3 discriminator: the leader a committer selects when
    // advancing to a new epoch must depend on the mined chain tip it holds
    // locally in its token cache, so the next epoch's leader is only
    // knowable once this tip is produced. This committer holds TWO real
    // mined tips (two internally-consistent blocks with different contents)
    // and selects a different leader for each; the bug (leader seed bound to
    // the empty/stale persisted tip instead of the mined one) ignores the
    // local tip, so both would select the same leader and this test fails.
    //
    // A many-staker spread is used so the two distinct mined tips — which are
    // fixed hashes we cannot control — land on different stakers deterministically
    // rather than by luck of a 50/50 draw.
    fn committer_with_blocks(n_blocks: u64) -> Committer {
        let (committer, _registry, _dp) = make_committer_for_leader_test(
            b"leader".to_vec(),
            b"leader".to_vec(),
        );
        // 256 spread stakers (unique keys, 1 stake each) — the tip's derived
        // target is uniform over a large space, so two distinct tips almost
        // certainly select different leaders.
        for i in 0..256 {
            committer.stake_store.add_staker(vec![i as u8], 1);
        }
        let mut token = Token::new();
        token.id = vec![1];
        token.is_self_verified = true;
        token.environment_id = "test".to_string();
        let mut previous_hash: Vec<u8> = vec![];
        for _ in 0..n_blocks {
            let mut block = Block {
                signed_trans: SignedTransaction::test_transaction(),
                token_metadata: HashMap::new(),
                previous_hash,
                timestamp: 0,
                current_hash: vec![],
                finality_status: FinalityStatus::Optimistic,
                proposer_key: vec![],
                epoch_number: 0,
            };
            block.current_hash =
                BlockFactory::create_hash(&block).expect("well-formed test block hashes");
            previous_hash = block.current_hash.clone();
            token.blockchain.add_block(block);
        }
        committer.bootstrap_token(token);
        committer
    }

    // Same epoch (both advance 1 -> 2), same stake, same starting leader, but
    // different mined tips: one chain holds no block, one holds two blocks.
    let leader_empty = committer_with_blocks(0).advance_epoch().unwrap();
    let leader_two_blocks = committer_with_blocks(2).advance_epoch().unwrap();

    assert_ne!(
        leader_empty,
        leader_two_blocks,
        "leader must depend on the mined chain tip"
    );
}


#[tokio::test]
async fn advance_epoch_to_never_rewinds_or_reuses() {
    // AUDIT Phase 5.4 / H9 discriminator: `advance_epoch_to` is the single writer of the
    // epoch number. Two consecutive advances are strictly increasing (never reuse a number)
    // and the mirrored counter tracks the authoritative detector; and a target that does not
    // strictly exceed the current value is refused — a replayed/rewinding advance is a no-op,
    // never a rewind. Reverting the guard (`if stored >= new { return Ok(None) }`) lets a
    // seeded-ahead counter get overwritten, so this test fails on the buggy code.
    let (committer, _registry, _dp) =
        make_committer_for_leader_test(b"leader".to_vec(), b"leader".to_vec());
    // Spread stakers so each epoch's derived seed lands on a distinct leader.
    for i in 0..256 {
        committer.stake_store.add_staker(vec![i as u8], 1);
    }

    // Two sequential advances: 1 -> 2 -> 3, strictly increasing, both non-empty leaders.
    let leader_1 = committer.advance_epoch().unwrap().expect("advanced to 2");
    let epoch_1 = committer.current_epoch_number.load(Ordering::SeqCst);
    let leader_2 = committer.advance_epoch().unwrap().expect("advanced to 3");
    let epoch_2 = committer.current_epoch_number.load(Ordering::SeqCst);
    assert!(!leader_1.is_empty());
    assert!(!leader_2.is_empty());
    assert_eq!(epoch_1, 2);
    assert_eq!(epoch_2, 3);
    assert_ne!(epoch_1, epoch_2, "advances must never reuse an epoch number");

    // The detector is authoritative: the mirrored counter matches its epoch. Scoped in a
    // block so the guard is dropped before the next advance acquires the detector lock.
    {
        let guard = committer.epoch_detector.lock().await;
        assert_eq!(
            guard.as_ref().unwrap().current_epoch.epoch_number,
            epoch_2,
            "detector epoch must track the mirrored counter"
        );
    }

    // Seed the counter ahead of the detector — exactly the divergence the single writer
    // removes. A replayed advance to a stale target (epoch 4) must be refused, never
    // overwrite the counter back down to 4.
    committer.current_epoch_number.store(1000, Ordering::SeqCst);
    assert!(
        committer.advance_epoch().unwrap().is_none(),
        "advance to epoch 4 must be refused when the stored counter (1000) is ahead"
    );
    let guard = committer.epoch_detector.lock().await;
    assert_eq!(
        guard.as_ref().unwrap().current_epoch.epoch_number, 3,
        "a refused advance must not advance the detector"
    );
}


#[tokio::test]
async fn advance_epoch_to_surfaces_snapshot_save_error() {
    // AUDIT Phase 5.4 / M8 discriminator: a stake/executor snapshot-persistence failure must
    // surface as `SnapshotPersist`, not be swallowed into `Ok(None)`. This provider fails
    // `save_stake_snapshot`/`save_executor_set`; advancing to a new epoch must return that
    // error. Reverting the `.map_err(...)` on the saves (the old `let _ =`) makes the advance
    // return Ok(None), so the test fails on the buggy code.
    let dp = Arc::new(TestDataProvider::new().with_snapshot_save_failure(true));
    let (committer, _registry, _dp) =
        build_committer_for_leader_test(b"leader".to_vec(), b"leader".to_vec(), dp);
    committer.stake_store.add_staker(b"leader".to_vec(), 100);

    let result = committer.advance_epoch();
    assert!(
        matches!(
            result,
            Err(CommitterError::SnapshotPersist { kind: "stake", .. })
        ),
        "advance_epoch must surface the snapshot-persistence failure, got {:?}",
        result
    );
}
