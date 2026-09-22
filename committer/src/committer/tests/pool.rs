//! S5.3 committer-level tests for the global `ShieldedPool` seam: the H12
//! shielded extension (block-carried payload vs. the pending registry's
//! authenticated entry) and the pool's independent re-validation — the
//! discriminator that a sentinel-validated payload can never ride on a
//! tampered wire block, and that a garbage proof is rejected by the pool's
//! OWN deps even with a matching registry entry.
//!
//! Fast-suite scope: no live Halo2 proving here, so these tests exercise the
//! reject paths (H12 mismatch, stale nullifier, stale root, bad proof), which
//! need no proof. The advance/persist/rollback lockstep paths are covered by
//! the `#[ignore]`d live tests at the bottom of this module.

use super::helpers::*;
use super::super::*;
use pneumatic_core::transactions::ShieldedTransaction;
use std::sync::Arc;


/// A distinct dummy 32-byte field value from a tag byte. High bytes stay
/// zero so the value is a valid little-endian Pallas scalar.
fn dummy(b: u8) -> [u8; 32] {
    let mut d = [0u8; 32];
    d[0] = b;
    d
}

/// A structurally-valid shielded tx whose commitments decode to the on-curve
/// point (1,0) (32-byte little-endian x=1, y=0 — y² = x³ − x ⇒ (1,0) is on
/// the Pallas curve), so validation checks 1–3 run. The 64-byte garbage proof
/// fails check 4 (`InvalidShieldedProof`) — the variable under test.
fn fake_stx(
    id: &str,
    nullifier: [u8; 32],
    merkle_root: [u8; 32],
) -> ShieldedTransaction {
    ShieldedTransaction {
        id: id.to_string(),
        action: "ShieldedTransfer".to_string(),
        token_id: vec![1],
        spent_commitments: vec![dummy(1)],
        nullifiers: vec![nullifier],
        commitments: vec![dummy(1)],
        merkle_root,
        proof: vec![0u8; 64],
        note_ciphertexts: vec![vec![0u8; 16]],
        fee: 10,
    }
}

/// Standard pre-commit setup: a funded sender, a bootstrapped token with a
/// genesis chain, and a Validated pending-registry entry for `tx_id`.
async fn setup(
    tx_id: &str,
    pool: Arc<ShieldedPool>,
) -> (Arc<TestDataProvider>, CommittersAndRegistry) {
    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 1000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer_with_pool(dp.clone(), pool);

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    (dp, (committer, registry))
}

type CommittersAndRegistry = (Committer, Arc<PendingTransactionRegistry>);


/// A block carrying a shielded payload whose tx id has NO entry in the
/// pending registry's shielded parallel map is rejected by the H12 shielded
/// extension — fail closed, pool untouched.
#[tokio::test]
async fn h12_rejects_shielded_block_without_registry_entry() {
    let pool = Arc::new(ShieldedPool::new(10));
    let (_dp, (committer, registry)) = setup("tx_h12_absent", pool.clone()).await;
    let tx_id = "tx_h12_absent";
    make_validated_entry(&registry, tx_id, b"alice".to_vec());
    // Deliberately NO register_shielded — the parallel map is empty.

    let stx = fake_stx(tx_id, dummy(0x55), [0u8; 32]);
    let block = make_shielded_block_for_token(&committer, tx_id, b"alice".to_vec(), stx);
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(
        matches!(result, Err(CommitterError::TransactionPayloadMismatch(_))),
        "H12 shielded extension must reject a shielded block with no registry entry: {:?}",
        result.err(),
    );
    // The rejection happened before pool apply — nothing advanced.
    assert_eq!(pool.leaf_count(), 0);
    assert_eq!(pool.applied_count(), 0);
}


/// The wire block carries a shielded tx with the registry's id but DIFFERENT
/// field bytes (a tampered payload). The H12 shielded hash check compares
/// against the authenticated entry and rejects — fail closed, pool untouched.
#[tokio::test]
async fn h12_rejects_swapped_shielded_payload() {
    let pool = Arc::new(ShieldedPool::new(10));
    let (_dp, (committer, registry)) = setup("tx_h12_swap", pool.clone()).await;
    let tx_id = "tx_h12_swap";
    make_validated_entry(&registry, tx_id, b"alice".to_vec());
    // The authenticated entry — what the sentinel validated.
    let registry_stx = fake_stx(tx_id, dummy(0x55), [0u8; 32]);
    registry.register_shielded(&registry_stx).unwrap();

    // The wire block carries the SAME id but a different nullifier — a swap.
    let wire_stx = fake_stx(tx_id, dummy(0x77), [0u8; 32]);
    let block = make_shielded_block_for_token(&committer, tx_id, b"alice".to_vec(), wire_stx);
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(
        matches!(result, Err(CommitterError::TransactionPayloadMismatch(_))),
        "H12 must reject a swapped shielded payload: {:?}",
        result.err(),
    );
    assert_eq!(pool.leaf_count(), 0);
    assert_eq!(pool.applied_count(), 0);
}


/// H12 passes (block payload hash-matches the registry entry) but the pool's
/// OWN re-validation rejects the garbage proof. This is the S5.3
/// discriminator: a sentinel-validated payload never lets a bad proof through
/// — the committer re-checks with pool-owned deps.
#[tokio::test]
async fn recheck_rejects_bad_proof_despite_matching_registry_entry() {
    let pool = Arc::new(ShieldedPool::new(10));
    let (_dp, (committer, registry)) = setup("tx_recheck_proof", pool.clone()).await;
    let tx_id = "tx_recheck_proof";
    make_validated_entry(&registry, tx_id, b"alice".to_vec());
    // Identical entry — the H12 hash check passes.
    let stx = fake_stx(tx_id, dummy(0x55), [0u8; 32]);
    registry.register_shielded(&stx).unwrap();

    let block = make_shielded_block_for_token(&committer, tx_id, b"alice".to_vec(), stx.clone());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(
        matches!(result, Err(CommitterError::ShieldedProofInvalid { .. })),
        "the pool re-check must reject the garbage proof: {:?}",
        result.err(),
    );
    // Cause names check 4 specifically.
    if let Err(CommitterError::ShieldedProofInvalid { cause, .. }) = &result {
        assert!(cause.contains("InvalidShieldedProof"), "cause: {cause}");
    }
    assert_eq!(pool.leaf_count(), 0);
    assert_eq!(pool.applied_count(), 0);
}


/// H12 passes, but the nullifier is ALREADY in the pool (a double-spend
/// attempt from a block that references an already-spent note). The pool's
/// re-check rejects with `StaleNullifier` before any mutation.
#[tokio::test]
async fn recheck_rejects_stale_nullifier_at_commit() {
    let pool = Arc::new(ShieldedPool::new(10));
    let (_dp, (committer, registry)) = setup("tx_recheck_nullifier", pool.clone()).await;
    let tx_id = "tx_recheck_nullifier";

    // Pre-mark the nullifier — the note was already spent.
    pool.view_parts().0
        .mark_many_atomic(&[dummy(0x55)])
        .unwrap();

    make_validated_entry(&registry, tx_id, b"alice".to_vec());
    let stx = fake_stx(tx_id, dummy(0x55), [0u8; 32]);
    registry.register_shielded(&stx).unwrap();

    let block = make_shielded_block_for_token(&committer, tx_id, b"alice".to_vec(), stx);
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(
        matches!(result, Err(CommitterError::ShieldedProofInvalid { .. })),
        "the pool re-check must reject the spent nullifier: {:?}",
        result.err(),
    );
    if let Err(CommitterError::ShieldedProofInvalid { cause, .. }) = &result {
        assert!(cause.contains("StaleNullifier"), "cause: {cause}");
    }
    // Rejected before mutation: exactly the pre-marked nullifier, no leaves.
    assert_eq!(pool.leaf_count(), 0);
    assert_eq!(pool.applied_count(), 0);
}


/// H12 passes, but the referenced merkle root is unknown (or stale) against
/// the pool's own root history — `StaleMerkleRoot`, fail closed.
#[tokio::test]
async fn recheck_rejects_stale_root_at_commit() {
    let pool = Arc::new(ShieldedPool::new(10));
    let (_dp, (committer, registry)) = setup("tx_recheck_root", pool.clone()).await;
    let tx_id = "tx_recheck_root";

    make_validated_entry(&registry, tx_id, b"alice".to_vec());
    // Root 0x99 — not in the pool's root history (the history holds only the
    // genesis zero root), so check 3 fails.
    let stx = fake_stx(tx_id, dummy(0x55), dummy(0x99));
    registry.register_shielded(&stx).unwrap();

    let block = make_shielded_block_for_token(&committer, tx_id, b"alice".to_vec(), stx);
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(
        matches!(result, Err(CommitterError::ShieldedProofInvalid { .. })),
        "the pool re-check must reject the stale root: {:?}",
        result.err(),
    );
    if let Err(CommitterError::ShieldedProofInvalid { cause, .. }) = &result {
        assert!(cause.contains("StaleMerkleRoot"), "cause: {cause}");
    }
    assert_eq!(pool.leaf_count(), 0);
    assert_eq!(pool.applied_count(), 0);
}


// ---------------------------------------------------------------------------
// Live (benchmark-only) tests — real Halo2 proving, `#[ignore]`d per the
// roadmap's "proving is benchmark-only" rule. Run on demand with:
//
//   cargo test -p pneumatic_committer -- --ignored
//
// Each runs one (or two) live proves (~1 min each). They are the live form of
// the fast discriminators above: the advance/persist/reload, idempotent
// replay, cross-block double-spend, durability fail-save, and lockstep
// rollback paths that a garbage proof can never exercise.
// ---------------------------------------------------------------------------

/// Builds one real 1-in/1-out transfer (a live prove, ~1 min) and returns a
/// fully wired committer fixture: the pool seeded with the PRIOR commit's
/// delta (mirror tree + root state + recorded delta — the sentinel fixture
/// pattern, via `record_delta_for_tests`), a funded sender, a bootstrapped
/// token, a Validated registry entry + the authenticated shielded entry, and
/// the shielded block. The tx references the pool's CURRENT root, so check 3
/// passes with distance 0.
async fn live_fixture(out_value: u64, fee: u64) -> (
    Arc<TestDataProvider>,
    CommittersAndRegistry,
    Arc<ShieldedPool>,
    Block,
) {
    use pneumatic_core::shielded::{
        commit, root_to_bytes, IncrementalMerkleTree, ShieldedNote, DEFAULT_DEPTH,
    };
    use pneumatic_prover::{build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};
    use pasta_curves::pallas::Scalar as Fq;

    // A satisfiable transfer: value = out + fee, input note a leaf of a fresh tree.
    let input_note = ShieldedNote {
        value: out_value + fee,
        owner_pk: [1u8; 32],
        rho: Fq::from(1),
        rcm: Fq::from(2),
    };
    let spend_key = [0xABu8; 32];
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root, proof) = tree.append(&commit(&input_note));

    // Seed the pool with the prior commit's delta: the block that placed this
    // note's commitment in the pool. After this the pool's current root is
    // exactly `root` (the one the transfer proves against).
    let pool = Arc::new(ShieldedPool::new(10));
    pool.record_delta_for_tests(&pneumatic_core::data::AppliedPoolDelta {
        block_hash: vec![0xAA; 32],
        leaves: vec![root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&input_note)))],
        nullifiers: vec![],
        post_root: root_to_bytes(&root),
    })
    .expect("seed the prior delta");

    let recipient = ShieldedIdentity {
        spend: SpendKey::from_seed([2u8; 32]),
        identity: Ed25519Provider::generate(),
    };
    let (stx, _output_notes) = build_shielded_tx(
        &[1u8],
        &[input_note],
        &[&spend_key],
        &[proof],
        root_to_bytes(&root),
        &[NoteOutput::new(out_value, recipient)],
        fee,
    )
    .expect("the real prove must succeed");

    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 10_000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer_with_pool(dp.clone(), pool.clone());

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = stx.id.clone();
    make_validated_entry(&registry, &tx_id, b"alice".to_vec());
    registry.register_shielded(&stx).unwrap();
    let block = make_shielded_block_for_token(&committer, &tx_id, b"alice".to_vec(), stx);
    (dp, (committer, registry), pool, block)
}

/// Wrap a block in its TransactionCommit (token 1, the test env).
fn commit_for(block: &Block) -> TransactionCommit {
    TransactionCommit {
        trans_id: block.signed_trans.transaction_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block.clone(),
    }
}

/// Discriminator (live): a real, valid shielded commit advances the pool,
/// persists the durable record, and a fresh `ShieldedPool::load` from the
/// store reconstructs the identical state (the boot path a restart takes).
#[tokio::test]
#[ignore = "live halo2 prove (~1 min); run with --ignored"]
async fn live_commit_advances_pool_persists_and_reloads() {
    let (dp, (committer, _registry), pool, block) = live_fixture(90, 10).await;
    let commit = commit_for(&block);

    let leaves_before = pool.leaf_count();
    assert!(
        committer.check_and_commit_transaction_results(&commit, vec![]).await.is_ok(),
        "the valid shielded commit succeeds"
    );
    assert_eq!(pool.leaf_count(), leaves_before + 1, "the output commitment leaf is appended");
    assert_eq!(pool.applied_count(), 2, "seed delta + this commit's delta");

    // The durable record: the store now holds a state with the new leaf count.
    let stored = dp
        .get_shielded_pool("token")
        .expect("store read")
        .expect("state persisted");
    assert_eq!(stored.leaf_count, pool.leaf_count());

    // A fresh load (the boot path) reconstructs the identical pool.
    let reloaded =
        ShieldedPool::load(dp.as_ref(), "token", 10).expect("reload from store");
    assert_eq!(reloaded.leaf_count(), pool.leaf_count());
    assert_eq!(reloaded.current_root(), pool.current_root());
    assert_eq!(reloaded.applied_count(), pool.applied_count());
}

/// Discriminator (live, the idempotency property): a re-delivered commit of an
/// ALREADY-committed block (the finalizer's ack-timeout resend) applies
/// nothing — the replay short-circuits as `AlreadyApplied` — and the chain's
/// duplicate rejection (previous_hash no longer the tip) undoes NOTHING: the
/// previously-applied delta must survive a chain-append failure on a replay.
#[tokio::test]
#[ignore = "live halo2 prove (~1 min); run with --ignored"]
async fn live_idempotent_replay_reapplies_nothing() {
    let (dp, (committer, registry), pool, block) = live_fixture(90, 10).await;
    let tx_id = block.signed_trans.transaction_id.clone();
    let commit = commit_for(&block);

    let leaves_before = pool.leaf_count();
    assert!(
        committer.check_and_commit_transaction_results(&commit, vec![]).await.is_ok()
    );
    assert_eq!(pool.leaf_count(), leaves_before + 1);
    let applied_after_first = pool.applied_count();

    // Re-deliver the SAME commit (the entry is re-admitted as Validated; the
    // shielded parallel map is never evicted, so H12 passes again).
    make_validated_entry(&registry, &tx_id, b"alice".to_vec());
    let replay = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(replay.is_err(), "the chain rejects the duplicate append");

    // …and the pool is EXACTLY as after the first commit: no double-append,
    // no double-record, the original delta intact (NOT reverted).
    assert_eq!(pool.leaf_count(), leaves_before + 1, "no double-append on replay");
    assert_eq!(pool.applied_count(), applied_after_first, "no double-record on replay");
    assert!(
        pool.applied_count() >= 2,
        "the original delta survived the replay's failed append"
    );
    let _ = &dp;
}

/// Discriminator (live, the cross-block double-spend guard): two REAL
/// transfers built from ONE input note (the second proves against the same
/// root — the append-only tree keeps its membership proof valid). The first
/// commits; the second is rejected by the pool's re-check with
/// `StaleNullifier` — the spend was already recorded in a prior block.
#[tokio::test]
#[ignore = "live halo2 prove x2 (~2 min); run with --ignored"]
async fn live_cross_block_double_spend_rejected() {
    use pneumatic_core::shielded::{
        commit, root_to_bytes, IncrementalMerkleTree, ShieldedNote, DEFAULT_DEPTH,
    };
    use pneumatic_prover::{build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};
    use pasta_curves::pallas::Scalar as Fq;

    // ONE input note, spent by TWO transfers (different outputs → different
    // tx ids; same nullifier).
    let input_note = ShieldedNote {
        value: 100,
        owner_pk: [1u8; 32],
        rho: Fq::from(1),
        rcm: Fq::from(2),
    };
    let spend_key = [0xABu8; 32];
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root, proof) = tree.append(&commit(&input_note));

    let pool = Arc::new(ShieldedPool::new(10));
    pool.record_delta_for_tests(&pneumatic_core::data::AppliedPoolDelta {
        block_hash: vec![0xAA; 32],
        leaves: vec![root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&input_note)))],
        nullifiers: vec![],
        post_root: root_to_bytes(&root),
    })
    .expect("seed the prior delta");

    let recipient_a = ShieldedIdentity {
        spend: SpendKey::from_seed([2u8; 32]),
        identity: Ed25519Provider::generate(),
    };
    let recipient_b = ShieldedIdentity {
        spend: SpendKey::from_seed([3u8; 32]),
        identity: Ed25519Provider::generate(),
    };
    let (stx_a, _) = build_shielded_tx(
        &[1u8], &[input_note.clone()], &[&spend_key], &[proof.clone()],
        root_to_bytes(&root), &[NoteOutput::new(90, recipient_a)], 10,
    )
    .expect("prove A");
    let (stx_b, _) = build_shielded_tx(
        &[1u8], &[input_note.clone()], &[&spend_key], &[proof.clone()],
        root_to_bytes(&root), &[NoteOutput::new(85, recipient_b)], 15,
    )
    .expect("prove B");
    assert_ne!(stx_a.id, stx_b.id, "distinct outputs → distinct tx ids");
    assert_eq!(stx_a.nullifiers, stx_b.nullifiers, "same input note → same nullifier");
    let nullifier = stx_a.nullifiers[0];
    let tx_a_id = stx_a.id.clone();
    let tx_b_id = stx_b.id.clone();

    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 10_000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer_with_pool(dp, pool.clone());
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // First spend — valid.
    make_validated_entry(&registry, &tx_a_id, b"alice".to_vec());
    registry.register_shielded(&stx_a).unwrap();
    let block_a = make_shielded_block_for_token(&committer, &tx_a_id, b"alice".to_vec(), stx_a);
    let commit_a = commit_for(&block_a);
    assert!(
        committer.check_and_commit_transaction_results(&commit_a, vec![]).await.is_ok(),
        "the first spend commits"
    );

    // Second spend of the SAME note — a double-spend.
    make_validated_entry(&registry, &tx_b_id, b"alice".to_vec());
    registry.register_shielded(&stx_b).unwrap();
    let block_b = make_shielded_block_for_token(&committer, &tx_b_id, b"alice".to_vec(), stx_b);
    let commit_b = commit_for(&block_b);
    let result = committer.check_and_commit_transaction_results(&commit_b, vec![]).await;

    assert!(
        matches!(result, Err(CommitterError::ShieldedProofInvalid { .. })),
        "the double-spend is rejected: {result:?}"
    );
    if let Err(CommitterError::ShieldedProofInvalid { cause, .. }) = &result {
        assert!(cause.contains("StaleNullifier"), "cause: {cause}");
    }
    // Exactly one spend is on the chain / in the pool.
    assert_eq!(
        committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
        2,
        "genesis + the first spend only"
    );
    assert!(
        pool.nullifiers().contains(nullifier),
        "the first spend's nullifier stays marked"
    );
}

/// Discriminator (live, the durability fail-save): when the durable
/// nullifier record CANNOT be written, the commit fails closed — the chain
/// append never happens and the just-applied delta is reverted (pool back to
/// its pre-commit state). A block is never reported committed without its
/// durable record.
#[tokio::test]
#[ignore = "live halo2 prove (~1 min); run with --ignored"]
async fn live_durability_fail_save_rolls_back() {
    use pneumatic_core::shielded::{
        commit, root_to_bytes, IncrementalMerkleTree, ShieldedNote, DEFAULT_DEPTH,
    };
    use pneumatic_prover::{build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};
    use pasta_curves::pallas::Scalar as Fq;

    let input_note = ShieldedNote {
        value: 100,
        owner_pk: [1u8; 32],
        rho: Fq::from(1),
        rcm: Fq::from(2),
    };
    let spend_key = [0xABu8; 32];
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root, proof) = tree.append(&commit(&input_note));

    let pool = Arc::new(ShieldedPool::new(10));
    pool.record_delta_for_tests(&pneumatic_core::data::AppliedPoolDelta {
        block_hash: vec![0xAA; 32],
        leaves: vec![root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&input_note)))],
        nullifiers: vec![],
        post_root: root_to_bytes(&root),
    })
    .expect("seed the prior delta");

    let recipient = ShieldedIdentity {
        spend: SpendKey::from_seed([2u8; 32]),
        identity: Ed25519Provider::generate(),
    };
    let (stx, _) = build_shielded_tx(
        &[1u8], &[input_note], &[&spend_key], &[proof],
        root_to_bytes(&root), &[NoteOutput::new(90, recipient)], 10,
    )
    .expect("the real prove must succeed");

    // The data service refuses the pool save.
    let dp = Arc::new(TestDataProvider::new().with_shielded_save_failure(true));
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 10_000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer_with_pool(dp, pool.clone());
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);
    let nullifier = stx.nullifiers[0];
    let tx_id = stx.id.clone();
    make_validated_entry(&registry, &tx_id, b"alice".to_vec());
    registry.register_shielded(&stx).unwrap();
    let block = make_shielded_block_for_token(&committer, &tx_id, b"alice".to_vec(), stx);
    let commit = commit_for(&block);

    let chain_before = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
    let leaves_before = pool.leaf_count();
    let applied_before = pool.applied_count();

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(
        matches!(result, Err(CommitterError::PoolPersist { .. })),
        "the save failure surfaces as PoolPersist: {result:?}"
    );
    // Fail closed: no chain append…
    assert_eq!(
        committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
        chain_before,
        "the chain append never happened"
    );
    // …and the applied delta was reverted (exact inverse).
    assert_eq!(pool.leaf_count(), leaves_before, "delta reverted");
    assert_eq!(pool.applied_count(), applied_before, "delta unrecorded");
    assert!(
        !pool.nullifiers().contains(nullifier),
        "the nullifier is unmarked again"
    );
}

/// Discriminator (live, lockstep): the FULL rollback property. Block A
/// commits (its delta applied + persisted). Sibling block B (higher stake)
/// wins the conflict at the same position: the commit path applies B's
/// delta, rolls A back on the CHAIN, and reverts A's delta from the POOL —
/// so after the commit, pool and chain agree exactly: only B's delta remains.
#[tokio::test]
#[ignore = "live halo2 prove x2 (~2 min); run with --ignored"]
async fn live_rollback_lockstep() {
    use pneumatic_core::shielded::{
        commit, root_to_bytes, IncrementalMerkleTree, ShieldedNote, DEFAULT_DEPTH,
    };
    use pneumatic_prover::{build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};
    use pasta_curves::pallas::Scalar as Fq;

    // TWO input notes in ONE tree: sibling proposals A and B each prove
    // against the same root R0 (both are members).
    let note_a = ShieldedNote {
        value: 100,
        owner_pk: [1u8; 32],
        rho: Fq::from(1),
        rcm: Fq::from(2),
    };
    let note_b = ShieldedNote {
        value: 100,
        owner_pk: [2u8; 32],
        rho: Fq::from(3),
        rcm: Fq::from(4),
    };
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    tree.append(&commit(&note_a)); // its fresh proof goes stale once note_b lands above it
    let (root, proof_b) = tree.append(&commit(&note_b));
    // note_a's membership re-derived against the FINAL root (the root both
    // transfers prove against — the pool's current root).
    let proof_a = tree.membership_proof(0);

    let pool = Arc::new(ShieldedPool::new(10));
    pool.record_delta_for_tests(&pneumatic_core::data::AppliedPoolDelta {
        block_hash: vec![0xAA; 32],
        leaves: vec![
            root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&note_a))),
            root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&note_b))),
        ],
        nullifiers: vec![],
        post_root: root_to_bytes(&root),
    })
    .expect("seed the prior delta");

    let spend_key = [0xABu8; 32];
    let recipient_a = ShieldedIdentity {
        spend: SpendKey::from_seed([2u8; 32]),
        identity: Ed25519Provider::generate(),
    };
    let recipient_b = ShieldedIdentity {
        spend: SpendKey::from_seed([3u8; 32]),
        identity: Ed25519Provider::generate(),
    };
    let (stx_a, _) = build_shielded_tx(
        &[1u8], &[note_a], &[&spend_key], &[proof_a],
        root_to_bytes(&root), &[NoteOutput::new(90, recipient_a)], 10,
    )
    .expect("prove A");
    let (stx_b, _) = build_shielded_tx(
        &[1u8], &[note_b], &[&spend_key], &[proof_b],
        root_to_bytes(&root), &[NoteOutput::new(90, recipient_b)], 10,
    )
    .expect("prove B");

    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 10_000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer_with_pool(dp, pool.clone());
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // Build BOTH sibling blocks BEFORE committing (both chain off the genesis tip).
    let nullifier_a = stx_a.nullifiers[0];
    let nullifier_b = stx_b.nullifiers[0];
    let tx_a_id = stx_a.id.clone();
    let tx_b_id = stx_b.id.clone();
    make_validated_entry(&registry, &tx_a_id, b"alice".to_vec());
    registry.register_shielded(&stx_a).unwrap();
    let block_a = make_shielded_block_for_token(&committer, &tx_a_id, b"alice".to_vec(), stx_a);
    make_validated_entry(&registry, &tx_b_id, b"alice".to_vec());
    registry.register_shielded(&stx_b).unwrap();
    let block_b = make_shielded_block_for_token(&committer, &tx_b_id, b"alice".to_vec(), stx_b);

    // A commits first (lower stake): it becomes the tip AND a candidate.
    committer.stake_store.add_staker(b"alpha".to_vec(), 100);
    let commit_a = commit_for(&block_a);
    assert!(
        committer
            .check_and_commit_transaction_results(&commit_a, b"alpha".to_vec())
            .await
            .is_ok(),
        "A commits and becomes the tip"
    );
    let tip_after_a = committer.tokens.get(&vec![1]).unwrap().value()
        .blockchain.get_current_chain_state().last_hash_in;
    assert_eq!(tip_after_a, commit_a.proposed_block.current_hash);
    assert!(
        pool.nullifiers().contains(nullifier_a),
        "A's delta is applied"
    );

    // B (higher stake) commits at the same position: conflict, B wins, A is
    // the rolled-back tip.
    committer.stake_store.add_staker(b"beta".to_vec(), 500);
    let commit_b = commit_for(&block_b);
    assert!(
        committer
            .check_and_commit_transaction_results(&commit_b, b"beta".to_vec())
            .await
            .is_ok(),
        "B wins the conflict and commits"
    );

    // Chain: A is gone, B is the sole block at the position.
    let tip = committer.tokens.get(&vec![1]).unwrap().value()
        .blockchain.get_current_chain_state().last_hash_in;
    assert_eq!(tip, commit_b.proposed_block.current_hash, "B is the tip");
    assert_eq!(
        committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
        2,
        "genesis + B: A rolled back"
    );

    // Pool: LOCKSTEP — A's delta is gone, B's delta remains.
    assert!(
        !pool.nullifiers().contains(nullifier_a),
        "A's nullifier is unmarked (delta reverted)"
    );
    assert!(
        pool.nullifiers().contains(nullifier_b),
        "B's nullifier is marked"
    );
    assert_eq!(
        pool.applied_count(),
        2,
        "seed delta + B's delta (A's delta reverted)"
    );
}
