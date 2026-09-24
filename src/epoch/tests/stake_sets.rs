//! Stake/executor-set tests: canonical-bytes order independence, totals
//! (saturating), missing-key reads, and the ExecutorSet→StakeSet
//! conversion.
use super::helpers::*;
use super::super::*;

// --- Phase 5.4 / H9+M8: snapshot integrity envelope ---

// A `StakeSet`'s canonical bytes are sorted (deterministic) so its SHA-256 digest is stable
// across save/load regardless of `HashMap` iteration order. `fingerprint()` is exactly
// SHA-256(canonical_bytes); two distinct payloads yield distinct fingerprints.
#[test]
fn stake_set_canonical_bytes_is_order_independent_and_fingerprint_matches_digest() {
    // Identical staker pairs inserted in DIFFERENT order -> byte-identical canonical bytes.
    let mut a = HashMap::new();
    a.insert(vec![1, 2, 3], 100);
    a.insert(vec![7], 200);
    let mut b = HashMap::new();
    b.insert(vec![7], 200);
    b.insert(vec![1, 2, 3], 100);
    let set_a = StakeSet { stakers: a };
    let set_b = StakeSet { stakers: b };
    assert_eq!(
        set_a.canonical_bytes().unwrap(),
        set_b.canonical_bytes().unwrap(),
        "canonical bytes must be independent of HashMap insertion order"
    );

    // fingerprint == sha256(canonical_bytes), and a changed payload changes the digest.
    let set = make_stake_set(vec![(vec![9], 55), (vec![8], 11)]);
    let digest = sha256(&set.canonical_bytes().unwrap());
    let fp: [u8; 32] = digest.as_slice().try_into().unwrap();
    assert_eq!(set.fingerprint(), fp, "fingerprint must equal SHA-256(canonical_bytes)");
    let set2 = make_stake_set(vec![(vec![9], 56), (vec![8], 11)]);
    assert_ne!(set.fingerprint(), set2.fingerprint(), "distinct payloads -> distinct fingerprints");
}

// `ExecutorSet` mirrors `StakeSet`: same helpers, same determinism.
#[test]
fn executor_set_canonical_bytes_is_order_independent() {
    let mut a = HashMap::new();
    a.insert(vec![1], 100);
    a.insert(vec![2], 200);
    let mut b = HashMap::new();
    b.insert(vec![2], 200);
    b.insert(vec![1], 100);
    let set_a = ExecutorSet { executors: a };
    let set_b = ExecutorSet { executors: b };
    assert_eq!(set_a.canonical_bytes().unwrap(), set_b.canonical_bytes().unwrap());
}

// --- StakeSet tests ---

#[test]
fn stake_set_total_stake_returns_sum() {
    let stakes = make_stake_set(vec![
        (vec![1], 10),
        (vec![2], 20),
        (vec![3], 30),
    ]);
    assert_eq!(stakes.total_stake(), 60);
}

// AUDIT Phase 6.9 (Item A, discriminator): two 2^63 stakes overflow u64::MAX.
// old .sum() panics in debug; saturating fold must return u64::MAX, no panic.
#[test]
fn stake_set_total_stake_saturates_on_overflow() {
    let large: u64 = 1 << 63;
    let stakes = make_stake_set(vec![(vec![1], large), (vec![2], large)]);
    assert_eq!(stakes.total_stake(), u64::MAX);
}

#[test]
fn stake_set_get_stake_returns_zero_for_missing_key() {
    let stakes = make_stake_set(vec![(vec![1], 10)]);
    assert_eq!(stakes.get_stake(&[2]), 0);
}

#[test]
fn stake_set_get_stake_returns_correct_value() {
    let stakes = make_stake_set(vec![(vec![1], 100)]);
    assert_eq!(stakes.get_stake(&[1]), 100);
}

// --- ExecutorSet tests ---

#[test]
fn executor_set_empty_on_creation() {
    let es = ExecutorSet::default();
    assert!(es.is_empty());
    assert_eq!(es.len(), 0);
    assert_eq!(es.total_stake(), 0);
}

#[test]
fn executor_set_total_stake_returns_sum() {
    let es = ExecutorSet {
        executors: [(b"a".to_vec(), 10), (b"b".to_vec(), 20), (b"c".to_vec(), 30)]
            .into_iter().collect(),
    };
    assert_eq!(es.total_stake(), 60);
    assert_eq!(es.len(), 3);
}

// AUDIT Phase 6.9 (Item A, discriminator): ExecutorSet::total_stake over 2^63 + 2^63
// overflows u64::MAX. old .sum() panics in debug; saturating fold must return u64::MAX.
#[test]
fn executor_set_total_stake_saturates_on_overflow() {
    let large: u64 = 1 << 63;
    let es = ExecutorSet {
        executors: [(b"a".to_vec(), large), (b"b".to_vec(), large)].into_iter().collect(),
    };
    assert_eq!(es.total_stake(), u64::MAX);
}

#[test]
fn executor_set_get_stake_returns_zero_for_missing_key() {
    let es = ExecutorSet {
        executors: [(b"a".to_vec(), 100)].into_iter().collect(),
    };
    assert_eq!(es.get_stake(&b"z".to_vec()), 0);
}

#[test]
fn executor_set_to_stake_set_converts() {
    let es = ExecutorSet {
        executors: [(b"a".to_vec(), 100)].into_iter().collect(),
    };
    let ss = es.to_stake_set();
    assert_eq!(ss.total_stake(), 100);
}
