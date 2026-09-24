//! Leader/selection tests: weighted + deterministic leader picks, the
//! seeded Shuffler, deterministic_select(_shard) distribution and overflow
//! safety, and domain-tagged selection seeds.
use super::helpers::*;
use super::super::*;

#[test]
fn leader_selector_empty_stake_set_returns_empty() {
    let selector = LeaderSelector::new();
    let stakes = make_stake_set(vec![]);
    let leader = selector.select(&stakes, 1, &[]);
    assert!(leader.is_empty());
}

#[test]
fn leader_selector_single_staker_always_selected() {
    let selector = LeaderSelector::new();
    let key = vec![1, 2, 3];
    let stakes = make_stake_set(vec![(key.clone(), 100)]);
    // Run 10 times — single staker should always be selected
    for _ in 0..10 {
        assert_eq!(selector.select(&stakes, 1, &[]), key);
    }
}

#[test]
fn leader_selector_deterministic_different_epochs_can_differ() {
    let selector = LeaderSelector::new();
    let key_a = vec![1];
    let key_b = vec![2];
    let stakes = make_stake_set(vec![(key_a.clone(), 50), (key_b.clone(), 50)]);
    // Same epoch → same leader
    let leader_epoch1 = selector.select(&stakes, 1, &[]);
    assert_eq!(leader_epoch1, selector.select(&stakes, 1, &[]));
    // Different epochs → deterministic but may differ
    let leader_epoch2 = selector.select(&stakes, 2, &[]);
    // Either they happen to be the same (still deterministic), or differ
    // Just verify both calls with same epoch return the same result
    assert_eq!(leader_epoch1, selector.select(&stakes, 1, &[]));
    assert_eq!(leader_epoch2, selector.select(&stakes, 2, &[]));
}

#[test]
fn leader_selector_weighted_returns_more_from_larger_stake() {
    let selector = LeaderSelector::new();
    let key_small = vec![1];
    let key_large = vec![2];
    // Small gets 10%, large gets 90%
    let stakes = make_stake_set(vec![(key_small.clone(), 10), (key_large.clone(), 90)]);
    let mut small_count = 0u64;
    for _ in 0..100 {
        if selector.select(&stakes, 1, &[]) == key_small {
            small_count += 1;
        }
    }
    // Small should be selected ~10% of the time
    assert!(small_count <= 25, "expected ~10 small selections, got {}", small_count);
}

// --- SA_02: Deterministic leader selection ---

#[test]
fn leader_selector_deterministic_same_inputs_same_output() {
    let selector = LeaderSelector::new();
    let stakes = make_stake_set(vec![(vec![1], 30), (vec![2], 50), (vec![3], 20)]);
    let first = selector.select(&stakes, 5, &[]);
    for _ in 1..20 {
        assert_eq!(selector.select(&stakes, 5, &[]), first);
    }
}

// --- deterministic_select tests ---

#[test]
fn deterministic_select_empty_returns_none() {
    let stakes = make_stake_set(vec![]);
    let result = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx1", 1, &[]);
    assert!(result.is_none());
}

#[test]
fn deterministic_select_single_staker_always_same() {
    let key = vec![1, 2, 3];
    let stakes = make_stake_set(vec![(key.clone(), 100)]);
    for _ in 0..20 {
        assert_eq!(deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx1", 1, &[]), Some(key.clone()));
    }
}

#[test]
fn deterministic_select_different_txs_distribute() {
    let key_a = vec![1];
    let key_b = vec![2];
    let stakes = make_stake_set(vec![(key_a.clone(), 10), (key_b.clone(), 90)]);

    // Pick many different tx_ids — verify distribution roughly matches stake weights
    let mut a_count = 0u64;
    let num_trials = 200;
    for i in 0..num_trials {
        let tx_id = format!("tx_{}", i);
        if deterministic_select(&stakes, FINALIZER_DOMAIN, tx_id.as_bytes(), 1, &[]) == Some(key_a.clone()) {
            a_count += 1;
        }
    }
    // key_a has 10% stake — expect ~10% selections, with some tolerance
    assert!(a_count <= 30, "expected ≤30 small selections (10%), got {}", a_count);
    assert!(a_count >= 2, "expected ≥2 small selections (10%), got {}", a_count);
}

#[test]
fn deterministic_select_deterministic_across_epochs() {
    let stakes = make_stake_set(vec![(vec![1], 30), (vec![2], 50), (vec![3], 20)]);
    let epoch1 = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx_alpha", 1, &[]);
    let epoch1_again = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx_alpha", 1, &[]);
    assert_eq!(epoch1, epoch1_again); // Same seed + same epoch → same result

    let epoch2 = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx_alpha", 2, &[]);
    // Epoch2 may or may not differ — but must be deterministic
    let epoch2_again = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx_alpha", 2, &[]);
    assert_eq!(epoch2, epoch2_again);
}

#[test]
fn deterministic_select_zero_stake_returns_none() {
    let stakes = make_stake_set(vec![(vec![1], 0), (vec![2], 0)]);
    let result = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx1", 1, &[]);
    assert!(result.is_none());
}

#[test]
fn deterministic_select_skips_zero_stake_key() {
    // AUDIT Phase 6.6: zero-stake `vec![0]` sorts first. With total==1 the random
    // target is forced to 0, which hits the zero key first (cumulative(0) >= target(0)).
    // The fix drops zero keys, so a positive-stake key is always returned.
    let stakes = make_stake_set(vec![(vec![0], 0), (vec![1], 1)]);
    for _ in 0..50 {
        let result = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx1", 1, &[]);
        assert_eq!(result, Some(vec![1]), "zero-stake key must never be selected");
    }

    // Mixed case: the zero key is neither-first nor alone — it must still be skipped.
    let stakes = make_stake_set(vec![(vec![0], 0), (vec![1], 30), (vec![2], 70)]);
    let mut saw_zero = false;
    for i in 0..200u8 {
        if deterministic_select(&stakes, FINALIZER_DOMAIN, &[i], 1, &[]).map(|k| k == vec![0]).unwrap_or(false)
        {
            saw_zero = true;
        }
    }
    assert!(!saw_zero, "zero-stake key must never be selected even in a mixed set");
}

// AUDIT Phase 6.9 (Item A, discriminator): cumulative walk over two 2^63 stakes
// would overflow u64. old `cumulative += stake` panics in debug; checked_add must
// resolve the walk to Some(_), no panic.
#[test]
fn deterministic_select_no_panic_on_overflowing_stakes() {
    let large: u64 = 1 << 63;
    let stakes = make_stake_set(vec![(vec![1], large), (vec![2], large)]);
    let mut saw_zero = false;
    for i in 0..200u8 {
        match deterministic_select(&stakes, FINALIZER_DOMAIN, &[i], 1, &[]) {
            Some(k) => assert!(!saw_zero || k == vec![1] || k == vec![2], "overflow walk picks a real key"),
            None => panic!("overflow set has total>0, selection must not be None"),
        }
    }
}

// --- Shuffler tests ---

#[test]
fn shuffler_empty_returns_empty() {
    let shuffler = Shuffler::new(vec![], 1, &[]);
    let shuffled = shuffler.shuffle();
    assert!(shuffled.is_empty());
}

#[test]
fn shuffler_single_item_same_order() {
    let shuffler = Shuffler::new(vec![b"executor_1".to_vec()], 1, &[]);
    let shuffled = shuffler.shuffle();
    assert_eq!(shuffled.len(), 1);
    assert_eq!(shuffled[0], b"executor_1".to_vec());
}

#[test]
fn shuffler_deterministic_same_epoch_same_order() {
    let keys = vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()];
    let shuffler1 = Shuffler::new(keys.clone(), 42, &[]);
    let shuffler2 = Shuffler::new(keys, 42, &[]);
    let result1 = shuffler1.shuffle();
    let result2 = shuffler2.shuffle();
    assert_eq!(result1.len(), result2.len());
    for (a, b) in result1.iter().zip(result2.iter()) {
        assert_eq!(a, b);
    }
}

#[test]
fn shuffler_different_epochs_different_order() {
    let keys = vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec(), b"e".to_vec()];
    let shuffler1 = Shuffler::new(keys.clone(), 1, &[]);
    let shuffler2 = Shuffler::new(keys.clone(), 2, &[]);
    let result1 = shuffler1.shuffle();
    let result2 = shuffler2.shuffle();
    // Different seeds must produce different full permutations.
    assert_ne!(result1.to_vec(), result2.to_vec());
}

// ---------------------------------------------------------------------------
// Deterministic shard selection tests
// ---------------------------------------------------------------------------

#[test]
fn deterministic_select_shard_empty_returns_none() {
    let executors = ExecutorSet::default();
    let result = deterministic_select_shard(&executors, 2, "tx1", 1, &[]);
    assert!(result.is_none());
}

#[test]
fn deterministic_select_shard_single_shard_returns_all() {
    let mut executors = ExecutorSet::default();
    executors.executors.insert(b"exec1".to_vec(), 100);
    executors.executors.insert(b"exec2".to_vec(), 200);
    let result = deterministic_select_shard(&executors, 1, "any-tx", 1, &[]);
    assert!(result.is_some());
    let keys = result.unwrap();
    assert_eq!(keys.len(), 2);
}

#[test]
fn deterministic_select_shard_excludes_zero_stake_executor() {
    // AUDIT Phase 6.6: zero-stake `vec![0]` must never be listed as a responsible
    // executor — not under the shard_count==1 shortcut, not under round-robin.
    let exec0 = vec![0];
    let exec1 = vec![1];
    let exec2 = vec![2];
    let es = ExecutorSet {
        executors: vec![(exec0.clone(), 0u64), (exec1.clone(), 100), (exec2.clone(), 100)]
            .into_iter()
            .collect(),
    };

    // shard_count == 1: the shortcut returns only the sorted positive-stake keys.
    let single = deterministic_select_shard(&es, 1, "any-tx", 1, &[]).unwrap();
    assert_eq!(single, vec![exec1.clone(), exec2.clone()]);
    assert!(!single.iter().any(|k| *k == exec0));

    // shard_count > 1: round-robin must never place the zero-stake executor in the
    // selected shard's key list (the result is a flat list of keys for that shard).
    // A shard may legitimately be empty (None) when only 2 positive executors are
    // spread over 3 shards — that is fine: nothing is assigned to it.
    for shard_count in 2..4u32 {
        for seed in 0..32u8 {
            if let Some(shards) = deterministic_select_shard(&es, shard_count, "tx-shard", 1, &[seed]) {
                assert!(
                    !shards.iter().any(|k| *k == exec0),
                    "zero-stake executor leaked into the selected shard (shard_count={})",
                    shard_count
                );
            }
        }
    }
}

#[test]
fn deterministic_select_shard_distributes_across_shards() {
    let mut executors = ExecutorSet::default();
    for i in 0..6 {
        executors.executors.insert(format!("exec{}", i).into_bytes(), 100);
    }
    // With 6 executors and 3 shards, each shard should get ~2 executors
    let result = deterministic_select_shard(&executors, 3, "tx-42", 1, &[]);
    assert!(result.is_some());
    let shard = result.unwrap();
    assert!(shard.len() >= 1 && shard.len() <= 4,
        "shard size {} should be between 1 and 4", shard.len());
}

// AUDIT Phase 6.9 (Item A, discriminator): round-robin stake accumulation over four
// 2^63 executors across 2 shards sums one shard to 2^64 (overflow). old `+= stake`
// panics in debug; checked_add(...) must saturate without panicking.
#[test]
fn deterministic_select_shard_no_panic_on_overflowing_stakes() {
    let large: u64 = 1 << 63;
    let mut executors = ExecutorSet::default();
    for i in 0..4 {
        executors.executors.insert(format!("exec{}", i).into_bytes(), large);
    }
    let result = deterministic_select_shard(&executors, 2, "tx-overflow", 1, &[]);
    assert!(result.is_some(), "overflowing-stake shard walk must still return a shard");
}

#[test]
fn deterministic_select_shard_deterministic_across_calls() {
    let mut executors = ExecutorSet::default();
    for i in 0..4 {
        executors.executors.insert(format!("exec{}", i).into_bytes(), 100 + i);
    }
    let result1 = deterministic_select_shard(&executors, 2, "same-tx", 5, &[]);
    let result2 = deterministic_select_shard(&executors, 2, "same-tx", 5, &[]);
    assert!(result1.is_some() && result2.is_some());
    assert_eq!(result1.unwrap(), result2.unwrap());
}

// --- C6: sort before shuffling ---

#[test]
fn deterministic_select_shard_sorted_before_shuffle() {
    // Same logical executor set, in different HashMap insertion orders and after a serde
    // round-trip, must yield identical shard partitions. Without the sort both the
    // shard_count==1 shortcut and the shuffle consumed HashMap iteration order, so two
    // nodes with the same executor set would route the same tx to different shards.
    fn build() -> ExecutorSet {
        let mut e = ExecutorSet::default();
        for i in 0..8 {
            e.executors.insert(format!("exec{i}").into_bytes(), 100 + i);
        }
        e
    }
    let forward = build();
    let mut reversed = build();
    reversed.executors.clear();
    for i in (0..8).rev() {
        reversed.executors.insert(format!("exec{i}").into_bytes(), 100 + i);
    }
    let bytes = crate::encoding::serialize_to_bytes_rmp(&forward).unwrap();
    let roundtrip: ExecutorSet = crate::encoding::deserialize_rmp_to(&bytes).unwrap();

    // shard_count==1 hits the shortcut path (full sorted set); the >1 cases hit the
    // shuffle path. Several (shard_count, tx, epoch) tuples cover the round-robin.
    let cases = [(1u32, "tx-a", 7), (3, "tx-a", 7), (4, "other-tx", 9), (2, "tx-a", 7)];
    for (sc, tx, ep) in cases {
        let a = deterministic_select_shard(&forward, sc, tx, ep, &[]).unwrap();
        let b = deterministic_select_shard(&reversed, sc, tx, ep, &[]).unwrap();
        let c = deterministic_select_shard(&roundtrip, sc, tx, ep, &[]).unwrap();
        assert_eq!(a, b, "forward vs reversed differ: shard_count={} tx={} epoch={}", sc, tx, ep);
        assert_eq!(a, c, "forward vs round-trip differ: shard_count={} tx={} epoch={}", sc, tx, ep);
    }
}

// --- Phase 5.3 / AUDIT H3: unpredictable selection seeds ---
//
// Regression discriminators for binding every selection seed to
// `prev_block_hash` (plus a per-type domain byte). Each must FAIL if the seed
// is reverted to depend only on `epoch_number`, which would make the next
// leader / shard / finalizer predictable from the public stake set.

#[test]
fn selection_seed_leader_changes_with_prev_block_hash() {
    // HEADLINE discriminator: same stake set + epoch, different prev_block_hash
    // → different leader. Without this binding an attacker can precompute the
    // next leader and pre-target it before it ever appears.
    let selector = LeaderSelector::new();
    let stakes = make_stake_set(vec![(vec![1], 50), (vec![2], 50)]);
    let leader_a = selector.select(&stakes, 7, &[0x11u8; 32]);
    let leader_b = selector.select(&stakes, 7, &[0x22u8; 32]);
    assert_ne!(leader_a, leader_b, "leader must vary with prev_block_hash");
}

#[test]
fn selection_seed_distinct_domains_differ() {
    // Same stake set + epoch + prev_block_hash, but the LEADER vs FINALIZER
    // domain bytes must land on different nodes — a leader seed must never be
    // replayable as a finalizer seed. The seed-level split is the rigorous
    // proof (the domain byte is hashed into the seed); the selection-level
    // split over a spread stake set shows it end-to-end.
    let stakes = make_stake_set(vec![(vec![1], 10), (vec![2], 30), (vec![3], 60)]);
    let prev = [0x33u8; 32];

    // Seed-level: the domain byte is part of the hashed input, so two
    // selections over the same snapshot derive from different seeds.
    let leader_seed = derive_selection_seed(LEADER_DOMAIN, 7, &prev, &[]);
    let finalizer_seed = derive_selection_seed(FINALIZER_DOMAIN, 7, &prev, b"tx1");
    assert_ne!(leader_seed, finalizer_seed, "domains must be separated in the seed");

    // Selection-level: the different seeds land on different stake keys.
    let leader = deterministic_select(&stakes, LEADER_DOMAIN, &[], 7, &prev).unwrap();
    let finalizer = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx1", 7, &prev).unwrap();
    assert_ne!(leader, finalizer, "leader and finalizer must not collide");
}

#[test]
fn selection_seed_shard_index_changes_with_prev_block_hash() {
    // The same tx in the same epoch routes to a different shard partition when
    // the previous block hash differs — no pre-targeting of the assigned shard.
    fn build() -> ExecutorSet {
        let mut e = ExecutorSet::default();
        e.executors.insert(b"exec0".to_vec(), 100);
        e.executors.insert(b"exec1".to_vec(), 100);
        e.executors.insert(b"exec2".to_vec(), 100);
        e.executors.insert(b"exec3".to_vec(), 100);
        e
    }
    let executors = build();
    let a = deterministic_select_shard(&executors, 2, "tx-1", 7, &[0x11u8; 32]).unwrap();
    let b = deterministic_select_shard(&executors, 2, "tx-1", 7, &[0x22u8; 32]).unwrap();
    assert_ne!(a, b, "selected shard must vary with prev_block_hash");
}

#[test]
fn selection_seed_matches_manual_hash() {
    // Guards the exact byte layout of the derived seed:
    //   SHA-256(domain ‖ epoch ‖ prev_block_hash ‖ extra)
    let prev = [0x44u8; 32];
    let extra = b"tx1";
    let built = derive_selection_seed(LEADER_DOMAIN, 7, &prev, extra);
    let mut input = Vec::new();
    input.push(LEADER_DOMAIN);
    input.extend_from_slice(&7u64.to_be_bytes());
    input.extend_from_slice(&prev);
    input.extend_from_slice(extra);
    let digest = ring::digest::digest(&ring::digest::SHA256, &input);
    let mut expected = [0u8; 32];
    expected.copy_from_slice(digest.as_ref());
    assert_eq!(built, expected, "derived seed must equal manual SHA-256(domain ‖ epoch ‖ prev ‖ extra)");
}

#[test]
fn selection_seed_independent_of_tx_id_for_leader() {
    // The leader path carries no per-tx extra, so it is one stable leader for
    // the stake set regardless of transaction. The finalizer path salts on
    // tx_id, so across transactions it does NOT pin every tx to a single
    // finalizer — dropping tx_id (the regression) would route all of them to
    // one node and wreck load distribution. Proves `extra` is used by the
    // finalizer/shard-index paths, not the leader.
    let key_a = vec![1];
    let key_b = vec![2];
    let stakes = make_stake_set(vec![(key_a.clone(), 10), (key_b.clone(), 90)]);
    let prev = [0x55u8; 32];

    // Leader: a single, stable key, independent of any transaction.
    let leader = deterministic_select(&stakes, LEADER_DOMAIN, &[], 7, &prev).unwrap();
    assert_eq!(leader.len(), 1, "leader path returns exactly one stake key");

    // Finalizer: across many tx_ids, routing spans more than one key (tx_id
    // is a real salt). A regression that dropped tx_id would pin every tx to
    // the single leader key.
    let mut finalizer_keys = std::collections::BTreeSet::new();
    for i in 0..100 {
        let key = deterministic_select(
            &stakes,
            FINALIZER_DOMAIN,
            format!("tx_{i}").as_bytes(),
            7,
            &prev,
        )
        .unwrap();
        finalizer_keys.insert(key);
    }
    assert!(
        finalizer_keys.len() > 1,
        "finalizer must span multiple keys across txs (tx_id is a real salt); got {:?}",
        finalizer_keys
    );
}
