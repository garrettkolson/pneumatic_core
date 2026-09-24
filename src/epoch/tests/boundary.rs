//! EpochBoundaryDetector tests: expiry timing, current/empty leader, epoch
//! advance, and stale-block detection.
use super::helpers::*;
use super::super::*;

#[test]
fn detector_not_expired_before_end() {
    let epoch = make_epoch(1, 0, 1000, vec![1]);
    let detector = EpochBoundaryDetector::new(epoch);
    assert!(!detector.is_epoch_expired(999));
}

#[test]
fn detector_expired_at_end() {
    let epoch = make_epoch(1, 0, 1000, vec![1]);
    let detector = EpochBoundaryDetector::new(epoch);
    assert!(detector.is_epoch_expired(1000));
}

#[test]
fn detector_current_leader_returns_some() {
    let epoch = make_epoch(1, 0, 1000, vec![1]);
    let detector = EpochBoundaryDetector::new(epoch);
    let leader = detector.current_leader();
    assert!(matches!(leader, Some(bytes) if bytes == [1u8]));
}

#[test]
fn detector_current_leader_empty_returns_none() {
    let epoch = make_epoch(1, 0, 1000, vec![]);
    let detector = EpochBoundaryDetector::new(epoch);
    assert!(detector.current_leader().is_none());
}

#[test]
fn detector_advance_to_new_epoch_bumps_number_and_selects_leader() {
    let epoch = make_epoch(1, 0, 1000, vec![1]);
    let mut detector = EpochBoundaryDetector::new(epoch);
    let selector = LeaderSelector::new();
    let stakes = make_stake_set(vec![(vec![2], 100)]);
    detector.advance_to_new_epoch(&selector, &stakes, 1000, &[]);
    assert_eq!(detector.current_epoch.epoch_number, 2);
    assert_eq!(detector.previous_leader, Some(vec![1]));
    assert_eq!(detector.current_epoch.leader_public_key, vec![2]);
}

#[test]
fn detector_is_stale_block_detects_previous_leader() {
    let epoch = make_epoch(1, 0, 1000, vec![1]);
    let mut detector = EpochBoundaryDetector::new(epoch);
    detector.previous_leader = Some(vec![99]);
    assert!(detector.is_stale_block(&vec![99]));
    assert!(!detector.is_stale_block(&vec![1]));
}

#[test]
fn detector_is_stale_block_no_previous_returns_false() {
    let epoch = make_epoch(1, 0, 1000, vec![1]);
    let detector = EpochBoundaryDetector::new(epoch);
    assert!(!detector.is_stale_block(&vec![1]));
    assert!(!detector.is_stale_block(&vec![99]));
}
