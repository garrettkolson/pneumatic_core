//! Phase S6.2 — adversarial shielded-transfer suite at the CONSENSUS (pool)
//! layer.
//!
//! ## Audit result: the classic attack vectors
//!
//! | # | Attack | Deciding layer | Canonical coverage |
//! |---|--------|----------------|--------------------|
//! | a | replay of an already-spent nullifier (serial) | sentinel advisory + committer re-check | sentinel `shielded_transfer_stale_nullifier_rejected_via_pool_view` (advisory); committer `recheck_rejects_stale_nullifier_at_commit` (authoritative) |
//! | b | concurrent double-spend of one note (race) | the pool's single writer | NEW: `concurrent_same_note_spend_exactly_one_commits` |
//! | c | stale Merkle root | sentinel advisory + committer re-check | sentinel `shielded_transfer_stale_root_rejected_via_pool_view`; committer `recheck_rejects_stale_root_at_commit` |
//! | d | truncated / malformed proof | the pool re-check, check 4 | NEW: `truncated_proof_rejected_not_panicked` |
//! | e | value imbalance (outputs > inputs) | the prover, off-circuit | prover `build_shielded_tx` value-balance pre-check (prover crate tests) |
//! | f | non-shielded / non-self-verified token | sentinel + finalizer gates | sentinel `shielded_transfer_not_opt_in_rejected`, `shielded_transfer_not_self_verified_rejected` |
//!
//! The two fast tests pin the POOL layer directly — checks 1→3 short-circuit
//! before check 4, so they run in the default suite with no keygen. The two
//! live tests need real proofs (the race needs BOTH contenders valid; the
//! truncated test must reach check 4), so this binary pays the one-time
//! ActionCircuit `keygen_vk` (~2.5 min) and both are `#[ignore]`d.

use std::collections::HashMap;
use std::sync::Arc;

use pneumatic_core::blocks::{Block, BlockFactory, FinalityStatus};
use pneumatic_core::data::{AppliedPoolDelta, ShieldedPoolState, StubDataProvider};
use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
use pneumatic_core::shielded::{
    commit, nullifier, root_to_bytes, IncrementalMerkleTree, ShieldedNote, DEFAULT_DEPTH,
};
use pneumatic_core::transactions::{ShieldedTransaction, SignedTransaction};
use pneumatic_committer::committer_error::CommitterError;
use pneumatic_committer::{PoolApplyOutcome, ShieldedPool};
use pneumatic_prover::{build_shielded_tx, create_note, NoteOutput, ShieldedIdentity, SpendKey};
use pasta_curves::group::GroupEncoding;
use pasta_curves::pallas::Scalar as Fq;

// ---------------------------------------------------------------------------
// Fixtures (public API only)
// ---------------------------------------------------------------------------

/// The test environment (the sentinel fixture's spec): the pool's re-check is
/// registry-independent (`ShieldedValidationSpec::new()` is built directly),
/// so no shielded spec registration is needed here.
fn test_env() -> Arc<EnvironmentMetadata> {
    let json = r#"{"environment_id":"test","environment_name":"test",
        "partitions":[{"id":"token","partition_type":"Token"},
        {"id":"slush","partition_type":"Slush"}],
        "asym_crypto_provider":{"Ed25519":null},"sym_crypto_provider":"sym",
        "serialization_provider":"rmp","quorum_percentage":67.0,
        "override_quorum_percentage":0.0,"max_risk":1.0,
        "allowed_token_types":[],"trans_validation_specs":[],
        "block_validation_specs":[],"log_file":"test.log"}"#;
    let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).unwrap();
    Arc::new(EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec"))
}

/// A 32-byte wire commitment from a REAL Pedersen commitment of a note (the
/// S4.1 canonical fixture pattern): `commit(note).to_bytes()` is a canonical
/// compressed on-curve point, so `public_inputs_from_shielded_tx` decodes it
/// and checks 2→4 can run. (An arbitrary 32-byte constant generally encodes a
/// non-member — the verifier decodes fail-closed as `InvalidCommitment`.)
fn real_commit(note: &ShieldedNote) -> [u8; 32] {
    commit(note).to_bytes().as_ref().try_into().unwrap()
}

/// The S4.1 canonical output note (value 90, owner [2u8; 32]).
fn output_note() -> ShieldedNote {
    ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3), rcm: Fq::from(4) }
}

/// The canonical S4.1 input note (the validation/canonical fixture): value
/// 100, owner [1u8; 32].
fn input_note() -> ShieldedNote {
    ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) }
}

/// The leaf bytes a note's commitment becomes in the pool tree.
fn input_leaf(note: &ShieldedNote) -> [u8; 32] {
    let c = commit(note);
    root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&c))
}

/// A pool loaded from a persisted state carrying the input note's single leaf
/// and (optionally) a prior spend of `pre_spent` (with the matching applied
/// delta — `load` asserts stored nullifiers == the deltas' nullifier union).
fn pool_with_leaf(input: &ShieldedNote, pre_spent: Option<[u8; 32]>) -> Arc<ShieldedPool> {
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root, _) = tree.append(&commit(input));
    let root_bytes = root_to_bytes(&root);
    let leaf = input_leaf(input);
    // `load` rebuilds the leaf sequence FROM the applied deltas (the
    // documented derivation), so even a leaf with no prior spend needs its
    // seed delta.
    let applied = vec![AppliedPoolDelta {
        block_hash: vec![99u8; 32],
        leaves: vec![leaf],
        nullifiers: pre_spent.iter().copied().collect(),
        post_root: root_bytes,
    }];
    let nullifiers: Vec<[u8; 32]> = pre_spent.iter().copied().collect();
    let state = ShieldedPoolState {
        root: root_bytes,
        leaf_count: 1,
        leaves: vec![leaf],
        nullifiers,
        applied,
    };
    let dp = StubDataProvider::new().with_shielded_pool(state);
    ShieldedPool::load(&dp, "token", 10).expect("seeded pool loads")
}

/// A block carrying `stx` as its shielded payload. `timestamp` disambiguates
/// the block hash (the idempotency key).
fn shielded_block(stx: &ShieldedTransaction, timestamp: i64) -> Block {
    let mut signed = SignedTransaction::test_transaction();
    signed.transaction_id = stx.id.clone();
    signed.shielded = Some(stx.clone());
    let mut block = Block {
        signed_trans: signed,
        token_metadata: HashMap::new(),
        previous_hash: vec![42u8; 32],
        timestamp,
        current_hash: vec![],
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash = BlockFactory::create_hash(&block).expect("block hashes");
    block
}

// ---------------------------------------------------------------------------
// Fast: the re-check's early gates (no keygen — checks 1→3 short-circuit)
// ---------------------------------------------------------------------------

/// Attack (a) at the consensus layer: a block whose nullifier is ALREADY in
/// the pool is rejected at check 2 — before the proof check — and the pool is
/// untouched. The garbage 64-byte proof is irrelevant: check 2 fires first,
/// which is exactly why this test runs in the default suite.
#[test]
fn pool_rejects_stale_nullifier_at_apply() {
    let input = input_note();
    let spend_key = [0xABu8; 32];
    let n = nullifier(&input, &spend_key);
    let pool = pool_with_leaf(&input, Some(n));
    let env = test_env();

    // Re-spend of the same note (same nullifier), real commitments, root
    // still fresh — the replay attack, verbatim.
    let stx = pneumatic_prover::assemble_tx(
        &[1u8],
        vec![real_commit(&input)],
        vec![n],
        vec![real_commit(&output_note())],
        pool.state_snapshot().root,
        vec![0u8; 64],
        vec![],
        10,
    );
    let block = shielded_block(&stx, 1);

    let err = pool.apply_update(&block, &env).unwrap_err();
    let CommitterError::ShieldedProofInvalid { cause, .. } = &err else {
        panic!("expected ShieldedProofInvalid (the re-check's mapping), got: {err:?}");
    };
    assert!(
        cause.contains("StaleNullifier"),
        "check 2 must fire StaleNullifier before check 4; cause: {cause}"
    );
    // The rejection happens before any mutation.
    assert_eq!(pool.leaf_count(), 1);
    assert_eq!(pool.applied_count(), 1);
}

/// Attack (c) at the consensus layer: a block referencing a root the pool has
/// NEVER committed (a different tree's root) is rejected at check 3, pool
/// untouched. Fresh nullifier, so check 2 passes first — proving check 3,
/// not check 2, is the gate.
#[test]
fn pool_rejects_stale_root_at_apply() {
    let input = input_note();
    let pool = pool_with_leaf(&input, None);
    let env = test_env();

    // A second tree over TWO leaves: its root is a valid Pallas root the
    // pool has never seen.
    let mut other = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    other.append(&commit(&input));
    let second = ShieldedNote { value: 101, owner_pk: [2u8; 32], rho: Fq::from(5), rcm: Fq::from(6) };
    let (other_root, _) = other.append(&commit(&second));

    let stx = pneumatic_prover::assemble_tx(
        &[1u8],
        vec![real_commit(&input)],
        vec![[7u8; 32]], // fresh — check 2 passes
        vec![real_commit(&output_note())],
        root_to_bytes(&other_root), // never committed — check 3 must fire
        vec![0u8; 64],
        vec![],
        10,
    );
    let block = shielded_block(&stx, 1);

    let err = pool.apply_update(&block, &env).unwrap_err();
    let CommitterError::ShieldedProofInvalid { cause, .. } = &err else {
        panic!("expected ShieldedProofInvalid (the re-check's mapping), got: {err:?}");
    };
    assert!(
        cause.contains("StaleMerkleRoot"),
        "check 3 must fire StaleMerkleRoot; cause: {cause}"
    );
    assert_eq!(pool.leaf_count(), 1);
    assert_eq!(pool.applied_count(), 1, "only the seed delta; the attack added none");
}

// ---------------------------------------------------------------------------
// Live (keygen-paying, #[ignore])
// ---------------------------------------------------------------------------

/// Attack (d): a structurally-valid tx with a FRESH nullifier and a FRESH
/// root whose proof is truncated to one byte must fail check 4 with
/// `ShieldedProofInvalid` — and never panic (a malformed proof is an input to
/// the verifier's decode, not a memory-unsafety surface). Reaches check 4,
/// so it shares this binary's one-time keygen.
#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min) in this test binary. Run: `cargo test --test shielded_attacks -- --ignored truncated_proof_rejected_not_panicked`"]
fn truncated_proof_rejected_not_panicked() {
    let input = input_note();
    let pool = pool_with_leaf(&input, None);
    let env = test_env();
    let spend_key = [0xABu8; 32];

    let stx = pneumatic_prover::assemble_tx(
        &[1u8],
        vec![real_commit(&input)],
        vec![nullifier(&input, &spend_key)], // fresh
        vec![real_commit(&output_note())],
        pool.state_snapshot().root, // fresh
        vec![0u8; 1], // the attack: truncated proof
        vec![],
        10,
    );
    let block = shielded_block(&stx, 1);

    let err = pool.apply_update(&block, &env).unwrap_err();
    let CommitterError::ShieldedProofInvalid { cause, .. } = &err else {
        panic!("expected ShieldedProofInvalid, got: {err:?}");
    };
    assert!(
        cause.contains("InvalidShieldedProof"),
        "check 4 must reject the truncated proof as InvalidShieldedProof; cause: {cause}"
    );
    assert_eq!(pool.leaf_count(), 1);
    assert_eq!(pool.applied_count(), 1, "only the seed delta; the attack added none");
}

/// Attack (b) — THE race: two valid live proofs spending the SAME note
/// (same nullifier) land in two blocks that hit `apply_update`
/// concurrently. The pool's single-writer guard (re-check INCLUDED under the
/// guard) plus the atomic nullifier mark admit exactly ONE. Whichever wins,
/// the loser's re-check sees the spent nullifier and is rejected at check 2
/// — no double-spend, no partial state.
#[test]
#[ignore = "slow: two live ActionCircuit proves + the one-time ActionCircuit keygen_vk (~2.5 min) in this test binary. Run: `cargo test --test shielded_attacks -- --ignored concurrent_same_note_spend_exactly_one_commits`"]
fn concurrent_same_note_spend_exactly_one_commits() {
    let input = input_note();
    let spend_key = [0xABu8; 32];
    let nullifier = nullifier(&input, &spend_key);

    // One leaf in the tree; both contenders prove against that root.
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root, proof) = tree.append(&commit(&input));
    let root_bytes = root_to_bytes(&root);
    let pool = pool_with_leaf(&input, None);
    assert_eq!(pool.state_snapshot().root, root_bytes);
    let env = test_env();

    // Two DISTINCT valid spends of the same input (different recipients,
    // values, fees, output commitments, block hashes).
    let recipient_a = ShieldedIdentity {
        spend: SpendKey::from_seed([11u8; 32]),
        identity: pneumatic_core::crypto::Ed25519Provider::generate(),
    };
    let recipient_b = ShieldedIdentity {
        spend: SpendKey::from_seed([22u8; 32]),
        identity: pneumatic_core::crypto::Ed25519Provider::generate(),
    };
    let (stx_a, _) = build_shielded_tx(
        &[1u8],
        &[input.clone()],
        &[&spend_key],
        &[proof.clone()],
        root_bytes,
        &[NoteOutput::new(90, recipient_a)],
        10,
    )
    .expect("live prove A");
    let (stx_b, _) = build_shielded_tx(
        &[1u8],
        &[input.clone()],
        &[&spend_key],
        &[proof.clone()],
        root_bytes,
        &[NoteOutput::new(95, recipient_b)],
        5,
    )
    .expect("live prove B");
    assert_eq!(stx_a.nullifiers, stx_b.nullifiers, "same note ⇒ same nullifier");
    assert_ne!(stx_a.commitments, stx_b.commitments, "distinct outputs ⇒ distinct leaves");
    assert_ne!(stx_a.id, stx_b.id);

    let block_a = shielded_block(&stx_a, 1);
    let block_b = shielded_block(&stx_b, 2);
    assert_ne!(block_a.current_hash, block_b.current_hash);

    // The race: both apply under the pool's single guard.
    let pool_a = pool.clone();
    let pool_b = pool.clone();
    let env_a = env.clone();
    let env_b = env.clone();
    let t_a = std::thread::spawn(move || pool_a.apply_update(&block_a, &env_a));
    let t_b = std::thread::spawn(move || pool_b.apply_update(&block_b, &env_b));
    let res_a = t_a.join().unwrap();
    let res_b = t_b.join().unwrap();

    let mut applied = 0;
    let mut stale_rejected = 0;
    for res in [&res_a, &res_b] {
        match res {
            Ok(PoolApplyOutcome::Applied) => applied += 1,
            Ok(other) => panic!("unexpected outcome: {other:?}"),
            Err(CommitterError::ShieldedProofInvalid { cause, .. })
                if cause.contains("StaleNullifier") =>
            {
                stale_rejected += 1;
            }
            other => panic!("loser must be the check-2 stale-nullifier rejection, got: {other:?}"),
        }
    }
    assert_eq!(applied, 1, "exactly one of the two racers may commit the note");
    assert_eq!(stale_rejected, 1, "the loser must be rejected at check 2");

    // The final pool state: the winner's output leaf, the seed delta + ONE
    // applied delta, the nullifier spent.
    assert_eq!(pool.leaf_count(), 2);
    assert_eq!(pool.applied_count(), 2);
    let snap = pool.state_snapshot();
    assert!(snap.nullifiers.contains(&nullifier), "the raced nullifier is spent");
    // The loser's output must NOT be in the pool.
    let loser_commitments = if res_a.is_ok() { &stx_b.commitments } else { &stx_a.commitments };
    let winner_leaves: Vec<[u8; 32]> =
        snap.leaves.iter().copied().collect();
    for c in loser_commitments {
        // The loser's commitment encodes a point; its leaf is what an apply
        // would have appended — the pool's leaves must not contain it.
        let loser_leaf =
            pneumatic_core::shielded::commitment_leaf(c).expect("loser commitment decodes");
        assert!(
            !winner_leaves.contains(&root_to_bytes(&loser_leaf)),
            "the losing spend's output leaf must not have been appended"
        );
    }
}
