//! Epoch-type tests: `Epoch::new_with_leader` field wiring and the
//! `resolve_block_conflict` matrix (stake-difference discard, equal-stake
//! flag-both, same-proposer slash).
use super::helpers::*;
use super::super::*;

// --- Epoch::new_with_leader tests ---

#[test]
fn epoch_new_with_leader_sets_fields() {
    let selector = LeaderSelector::new();
    let key = vec![42];
    let stakes = make_stake_set(vec![(key.clone(), 100)]);
    let epoch = Epoch::new_with_leader(1, 1000, 2000, &selector, &stakes, &[]);
    assert_eq!(epoch.epoch_number, 1);
    assert_eq!(epoch.start_timestamp, 1000);
    assert_eq!(epoch.end_timestamp, 2000);
    assert_eq!(epoch.leader_public_key, key);
}

// --- resolve_block_conflict tests ---

#[test]
fn conflict_resolution_stake_difference_returns_discard_loser() {
    let stakes = make_stake_set(vec![(vec![1], 100), (vec![2], 200)]);
    let result = resolve_block_conflict(
        b"hash_a", b"hash_b",
        &vec![1], &vec![2],
        &stakes,
    ).unwrap();
    match result {
        ConflictResolution::DiscardLoser(winner) => assert_eq!(winner, b"hash_b"),
        _ => panic!("Expected DiscardLoser"),
    }
}

#[test]
fn conflict_resolution_equal_stake_different_proposers_returns_flag_both() {
    let stakes = make_stake_set(vec![(vec![1], 100), (vec![2], 100)]);
    // Equal stakes, different verified proposers: neither is out-staked, so resolve to
    // TieFlagBoth (fail-closed). This supersedes the old hash tie-break, which was
    // attacker-grindable.
    let result = resolve_block_conflict(
        b"aa", b"bb",
        &vec![1], &vec![2],
        &stakes,
    ).unwrap();
    match result {
        ConflictResolution::TieFlagBoth(_) => {}
        _ => panic!("Expected TieFlagBoth"),
    }
}

#[test]
fn conflict_resolution_same_proposer_returns_slash() {
    let stakes = make_stake_set(vec![(vec![1], 100)]);
    let result = resolve_block_conflict(
        b"hash_a", b"hash_b",
        &vec![1], &vec![1],
        &stakes,
    ).unwrap();
    match result {
        ConflictResolution::SameProposerSlash(winner, slashed) => {
            assert_eq!(winner, b"hash_a");
            assert_eq!(slashed, vec![1]);
        }
        _ => panic!("Expected SameProposerSlash"),
    }
}
