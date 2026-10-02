//! AUDIT Phase 7.2 — cross-process determinism fixture (closed 10/01/2026).
//!
//! The audit's guard: every consensus-visible choice must be a pure function
//! of logical content, never of process-local memory layout. Every selection
//! and hashing path was deliberately made insertion-order invariant
//! (`deterministic_select` sorts keys, `ExecutorSet::shuffler` sorts before
//! Fisher-Yates, canonical views route `HashMap`s through `BTreeMap`s). These
//! tests pin each of those invariants against a regression that reintroduces
//! per-process divergence — the failure mode where two honest nodes derive
//! different leaders/shards/hashes from identical chain state.
use super::super::*;
use crate::blocks::{Block, BlockFactory, FinalityStatus};
use crate::transactions::{SignedTransaction, Transaction, TransactionSignature};
use std::collections::HashMap as StdHashMap;

/// One canonical stake set, returned as pairs so callers can insert the same
/// logical content in different orders.
fn stake_pairs() -> Vec<(Vec<u8>, u64)> {
    vec![
        (b"alice".to_vec(), 100),
        (b"bob".to_vec(), 7),
        (b"carol".to_vec(), 42),
        (b"dave".to_vec(), 1),
        (b"erin".to_vec(), 250),
        (b"frank".to_vec(), 0), // zero-stake: must stay excluded, order-invariantly
        (b"gina".to_vec(), 13),
    ]
}

/// Same logical content, inserted in one of three orders (as-is / reversed /
/// rotated) — three HashMaps with (almost certainly) different iteration orders.
fn stake_set(order: usize) -> StakeSet {
    let pairs = stake_pairs();
    let ordered: Vec<(Vec<u8>, u64)> = match order % 3 {
        0 => pairs,
        1 => pairs.into_iter().rev().collect(),
        _ => { let mut p = pairs; p.rotate_left(3); p }
    };
    StakeSet { stakers: ordered.into_iter().collect() }
}

fn executor_pairs() -> Vec<(Vec<u8>, u64)> {
    vec![
        (b"exec-1".to_vec(), 10),
        (b"exec-2".to_vec(), 30),
        (b"exec-3".to_vec(), 5),
        (b"exec-4".to_vec(), 0), // slashed-to-zero: excluded from shards
        (b"exec-5".to_vec(), 77),
        (b"exec-6".to_vec(), 2),
    ]
}

fn executor_set(order: usize) -> ExecutorSet {
    let pairs = executor_pairs();
    let ordered: Vec<(Vec<u8>, u64)> = match order % 3 {
        0 => pairs,
        1 => pairs.into_iter().rev().collect(),
        _ => { let mut p = pairs; p.rotate_right(2); p }
    };
    ExecutorSet { executors: ordered.into_iter().collect() }
}

/// "Same stake set in different key orders → identical leader, shards, and
/// finalizer selection": sweep every domain × epoch × tip × salt combination
/// through the three insertion-order variants and require identical results.
#[test]
fn selection_is_invariant_to_stake_set_key_order() {
    let sets = [stake_set(0), stake_set(1), stake_set(2)];
    let domains = [
        LEADER_DOMAIN,
        FINALIZER_DOMAIN,
        SHARD_INDEX_DOMAIN,
        SHARD_SHUFFLE_DOMAIN,
    ];
    let epochs: [u64; 4] = [0, 1, 2, u64::MAX];
    let tips: [&[u8]; 3] = [&[], b"prev-block-hash-A", &[7u8; 32]];
    let extras: [&[u8]; 3] = [&[], b"tx-id-1", b"some other salt"];

    for domain in domains {
        for epoch in epochs {
            for tip in tips {
                for extra in extras {
                    let picked: Vec<Option<Vec<u8>>> = sets
                        .iter()
                        .map(|s| deterministic_select(s, domain, extra, epoch, tip))
                        .collect();
                    assert!(picked[0].is_some(), "positive total stake must yield a pick");
                    assert_eq!(
                        picked[0], picked[1],
                        "deterministic_select diverged on insertion order (domain={domain}, epoch={epoch})"
                    );
                    assert_eq!(picked[0], picked[2], "deterministic_select diverged (rotate order)");
                }
            }
        }
    }

    // The weighted LeaderSelector behind IEpochLeaderSelector: same sweep, coarser.
    for epoch in epochs {
        for tip in tips {
            let selector = LeaderSelector::new();
            let picks: Vec<Vec<u8>> = sets
                .iter()
                .map(|s| selector.select(s, epoch, tip))
                .collect();
            assert!(!picks[0].is_empty());
            assert_eq!(picks, vec![picks[0].clone(); 3], "leader selection diverged on key order");
        }
    }
}

/// Shard selection + the per-epoch rotation: identical partitions regardless
/// of ExecutorSet insertion order (the C6 invariant), for shard counts 1..3
/// and multiple transaction ids.
#[test]
fn shard_assignment_is_invariant_to_executor_set_key_order() {
    let sets = [executor_set(0), executor_set(1), executor_set(2)];
    for shard_count in [1u32, 2, 3] {
        for tx_id in ["tx-a", "tx-b", "0123456789abcdef"] {
            for (epoch, tip) in [(1u64, "tip1"), (99, "")] {
                let picks: Vec<Option<Vec<Vec<u8>>>> = sets
                    .iter()
                    .map(|s| deterministic_select_shard(s, shard_count, tx_id, epoch, tip.as_bytes()))
                    .collect();
                assert!(picks[0].is_some(), "positive-stake executors must populate shards");
                assert_eq!(
                    picks, vec![picks[0].clone(); 3],
                    "shard assignment diverged on insertion order (shards={shard_count}, tx={tx_id})"
                );
                // Zero-stake executors are excluded in every variant.
                assert!(picks[0].as_ref().unwrap().iter().all(|k| k != &b"exec-4".to_vec()));
            }
        }
    }

    // The rotation itself (per-epoch shuffle) is order-invariant through the
    // sorted-items guard in `ExecutorSet::shuffler`.
    for epoch in [0u64, 3, 77] {
        let perms: Vec<Vec<Vec<u8>>> = sets
            .iter()
            .map(|s| s.shuffler(epoch, b"tip").shuffle().to_vec())
            .collect();
        assert_eq!(perms, vec![perms[0].clone(); 3], "shuffle diverged on insertion order");
    }
}

/// Attested-snapshot lineage: the envelope fingerprint must be a function of
/// logical content only, or the save/load integrity check (H9/M8) would flag
/// honest round-trips as corruption across processes.
#[test]
fn canonical_bytes_and_fingerprint_are_insertion_order_invariant() {
    let sets = [stake_set(0), stake_set(1), stake_set(2)];
    let canonicals: Vec<Vec<u8>> = sets
        .iter()
        .map(|s| s.canonical_bytes().expect("canonicalization succeeds"))
        .collect();
    assert_eq!(canonicals[0], canonicals[1]);
    assert_eq!(canonicals[0], canonicals[2]);
    let prints: Vec<[u8; 32]> = sets.iter().map(|s| s.fingerprint()).collect();
    assert_eq!(prints[0], prints[1]);
    assert_eq!(prints[0], prints[2]);

    let exec_sets = [executor_set(0), executor_set(1), executor_set(2)];
    let ec: Vec<Vec<u8>> = exec_sets
        .iter()
        .map(|s| s.canonical_bytes().expect("canonicalization succeeds"))
        .collect();
    assert_eq!(ec[0], ec[1]);
    assert_eq!(ec[0], ec[2]);

    // Sanity: the fixture is not vacuous — different logical content differs.
    let mut other = stake_set(0);
    other.stakers.insert(b"zoe".to_vec(), 5);
    assert_ne!(other.fingerprint(), prints[0]);
}

// ---------------------------------------------------------------------------
// "Same logical block → identical hash": the two non-deterministic fields in
// the hashed form (Block::token_metadata, SignedTransaction::executor_sigs)
// are canonicalized (canonical_map_bytes / CanonicalSignedTransaction's
// BTreeMap). Pin that, plus the discriminating control.
// ---------------------------------------------------------------------------

fn tx_sig(n: u8) -> TransactionSignature {
    TransactionSignature {
        transaction_id: vec![n],
        env_id: b"env".to_vec(),
        transaction_hash: vec![n; 32],
        signature: vec![n; 64],
        current_stake: n as u64,
    }
}

fn signed_trans(sig_order: usize, meta_order: usize) -> SignedTransaction {
    let sigs: [(Vec<u8>, TransactionSignature); 3] = [
        (b"exec-1".to_vec(), tx_sig(1)),
        (b"exec-2".to_vec(), tx_sig(2)),
        (b"exec-3".to_vec(), tx_sig(3)),
    ];
    let mut executor_sigs: StdHashMap<Vec<u8>, TransactionSignature> = StdHashMap::new();
    match sig_order {
        0 => sigs.into_iter().for_each(|(k, v)| { executor_sigs.insert(k, v); }),
        1 => sigs.into_iter().rev().for_each(|(k, v)| { executor_sigs.insert(k, v); }),
        _ => { executor_sigs.insert(sigs[2].0.clone(), sigs[2].1.clone());
               executor_sigs.insert(sigs[0].0.clone(), sigs[0].1.clone());
               executor_sigs.insert(sigs[1].0.clone(), sigs[1].1.clone()); }
    }
    let _ = meta_order;
    SignedTransaction {
        transaction_id: "tx-determinism".to_string(),
        transaction: Transaction {
            id: "tx-determinism".to_string(),
            action: "Process".to_string(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b"sender".to_vec(),
            receiver: b"receiver".to_vec(),
            amount: Some(10),
            timestamp: 1000,
            result_hash: vec![],
            sender_signature: vec![],
            gas_limit: 10,
            payload: vec![],
            result_data: vec![],
        },
        total_stake: 6,
        total_voters: 3,
        leader_address: b"leader".to_vec(),
        leader_stake: 1,
        leader_hash: vec![9; 32],
        finalizer_addr: b"finalizer".to_vec(),
        finalizer_sig: tx_sig(9),
        executor_sigs,
        proposer_key: b"proposer".to_vec(),
        shielded: None,
    }
}

fn token_metadata(meta_order: usize) -> StdHashMap<String, String> {
    let pairs = vec![
        ("name".to_string(), "Pneumatic".to_string()),
        ("decimals".to_string(), "8".to_string()),
        ("ticker".to_string(), "PNEU".to_string()),
        ("issuer".to_string(), "acme".to_string()),
    ];
    let ordered: Vec<(String, String)> = match meta_order {
        0 => pairs,
        _ => pairs.into_iter().rev().collect(),
    };
    ordered.into_iter().collect()
}

fn block(sig_order: usize, meta_order: usize, timestamp: i64) -> Block {
    Block {
        signed_trans: signed_trans(sig_order, meta_order),
        token_metadata: token_metadata(meta_order),
        previous_hash: b"previous-hash".to_vec(),
        current_hash: vec![],
        timestamp,
        finality_status: FinalityStatus::Optimistic,
        proposer_key: b"proposer".to_vec(),
        epoch_number: 41,
    }
}

#[test]
fn same_logical_block_hashes_identically_across_serialization_orders() {
    let a = BlockFactory::create_hash(&block(0, 0, 1234)).expect("hash a");
    let b = BlockFactory::create_hash(&block(1, 1, 1234)).expect("hash b");
    let c = BlockFactory::create_hash(&block(2, 1, 1234)).expect("hash c");
    assert_eq!(a, b, "same logical block, different executor_sigs/token_metadata \
                      insertion orders, must hash identically");
    assert_eq!(a, c);

    // Discriminating controls — the equality above is not a constant hash.
    let d = BlockFactory::create_hash(&block(0, 0, 1235)).expect("hash d");
    assert_ne!(a, d, "timestamp is hashed content");
    let mut mutated = block(0, 0, 1234);
    mutated.token_metadata.insert("ticker".to_string(), "OTHER".to_string());
    let e = BlockFactory::create_hash(&mutated).expect("hash e");
    assert_ne!(a, e, "metadata values are hashed content");
    let mut dropped = block(0, 0, 1234);
    dropped.signed_trans.executor_sigs.remove(&b"exec-2".to_vec());
    let f = BlockFactory::create_hash(&dropped).expect("hash f");
    assert_ne!(a, f, "the executor signature set is hashed content");
}
