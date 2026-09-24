//! `NullifierRegistry` tests: single + atomic-batch marking, the
//! double-spend rejection contract, and the concurrent races (exactly one
//! winner, no partial batch).
use super::helpers::*;
use super::super::*;

// --- NullifierRegistry (Phase S4.2) ---

#[test]
fn try_mark_spent_first_mark_succeeds() {
    let registry = NullifierRegistry::new();
    let nullifier = [7u8; 32];
    assert!(registry.try_mark_spent(nullifier).is_ok());
    assert!(registry.contains(nullifier));
    assert_eq!(registry.len(), 1);
    assert!(!registry.is_empty());
}

#[test]
fn try_mark_spent_double_mark_rejected_as_stale_nullifier() {
    let registry = NullifierRegistry::new();
    let nullifier = [7u8; 32];
    registry.try_mark_spent(nullifier).unwrap();
    let err = registry.try_mark_spent(nullifier).unwrap_err();
    assert!(
        matches!(
            err,
            PneumaticError::Validation(ref reasons)
                if reasons == &vec![ValidationFailureReason::StaleNullifier]
        ),
        "double-mark must be Validation([StaleNullifier]) exactly, got: {:?}",
        err
    );
    // The rejected mark must not disturb the set.
    assert_eq!(registry.len(), 1);
    assert!(registry.contains(nullifier));
}

#[test]
fn contains_reflects_state() {
    let registry = NullifierRegistry::new();
    let nullifier = [9u8; 32];
    assert!(!registry.contains(nullifier));
    registry.try_mark_spent(nullifier).unwrap();
    assert!(registry.contains(nullifier));
}

#[test]
fn len_and_is_empty_track_inserts() {
    let registry = NullifierRegistry::new();
    assert_eq!(registry.len(), 0);
    assert!(registry.is_empty());
    registry.try_mark_spent([1u8; 32]).unwrap();
    registry.try_mark_spent([2u8; 32]).unwrap();
    registry.try_mark_spent([3u8; 32]).unwrap();
    assert_eq!(registry.len(), 3);
    assert!(!registry.is_empty());
}

#[test]
fn mark_many_atomic_all_fresh_marks_all() {
    let registry = NullifierRegistry::new();
    let a = [1u8; 32];
    let b = [2u8; 32];
    assert!(registry.mark_many_atomic(&[a, b]).is_ok());
    assert!(registry.contains(a));
    assert!(registry.contains(b));
    assert_eq!(registry.len(), 2);
}

#[test]
fn mark_many_atomic_second_already_spent_marks_none() {
    // All-or-nothing: a batch containing one already-spent nullifier marks
    // NONE of the others (roadmap 2.5 double-spend defense).
    let registry = NullifierRegistry::new();
    let a = [1u8; 32];
    let b = [2u8; 32];
    registry.try_mark_spent(b).unwrap();
    let err = registry.mark_many_atomic(&[a, b]).unwrap_err();
    assert!(
        matches!(
            err,
            PneumaticError::Validation(ref reasons)
                if reasons == &vec![ValidationFailureReason::StaleNullifier]
        ),
        "stale batch must be Validation([StaleNullifier]) exactly, got: {:?}",
        err
    );
    assert!(!registry.contains(a), "the fresh nullifier must NOT have been marked");
    assert_eq!(registry.len(), 1);
}

#[test]
fn mark_many_atomic_first_already_spent_rejects_before_any_insert() {
    // Check-all runs before insert-all: the trailing fresh nullifiers are
    // untouched even when the leading one is stale.
    let registry = NullifierRegistry::new();
    let a = [1u8; 32];
    let b = [2u8; 32];
    registry.try_mark_spent(a).unwrap();
    assert!(registry.mark_many_atomic(&[a, b]).is_err());
    assert!(!registry.contains(b));
    assert_eq!(registry.len(), 1);
}

#[test]
fn mark_many_atomic_duplicate_within_batch_rejected() {
    // A nullifier cannot be spent twice, not even by one tx: the phase-2
    // self-collision rolls back and nothing is marked.
    let registry = NullifierRegistry::new();
    let a = [1u8; 32];
    assert!(registry.mark_many_atomic(&[a, a]).is_err());
    assert!(!registry.contains(a));
    assert_eq!(registry.len(), 0);
}

#[test]
fn mark_many_atomic_empty_batch_is_noop() {
    let registry = NullifierRegistry::new();
    assert!(registry.mark_many_atomic(&[]).is_ok());
    assert_eq!(registry.len(), 0);
}

#[test]
fn concurrent_try_mark_spent_same_nullifier_exactly_one_succeeds() {
    // The atomic insert-returns-old idiom under a real race: N threads,
    // one nullifier, exactly one winner — no TOCTOU window.
    let registry = Arc::new(NullifierRegistry::new());
    let nullifier = [5u8; 32];
    let mut handles = vec![];
    for _ in 0..8 {
        let reg = registry.clone();
        handles.push(thread::spawn(move || reg.try_mark_spent(nullifier)));
    }
    let mut successes = 0;
    for h in handles {
        if h.join().unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(successes, 1, "exactly one thread may mark the nullifier");
    assert_eq!(registry.len(), 1);
}

#[test]
fn concurrent_mark_many_atomic_same_batch_one_succeeds_no_partial() {
    // The phase-1/phase-2 gap is the real race: 8 threads run the same
    // two-nullifier batch. All-or-nothing means the eventual state is the
    // whole batch, fully applied, with no partial state.
    let registry = Arc::new(NullifierRegistry::new());
    let a = [1u8; 32];
    let b = [2u8; 32];
    let mut handles = vec![];
    for _ in 0..8 {
        let reg = registry.clone();
        handles.push(thread::spawn(move || reg.mark_many_atomic(&[a, b])));
    }
    let mut successes = 0;
    for h in handles {
        if h.join().unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(successes, 1, "exactly one thread may win the batch");
    assert_eq!(registry.len(), 2, "the winning batch is fully applied");
    assert!(registry.contains(a));
    assert!(registry.contains(b));
}

#[test]
fn concurrent_mark_many_atomic_mixed_unique_and_duplicates() {
    // 16 threads: 8 unique single-nullifier batches + 8 duplicates over
    // the same 8 nullifiers. Exactly 8 wins, 8 stale rejections, 8 in the
    // set — the double-spend defense under real concurrency.
    let registry = Arc::new(NullifierRegistry::new());
    let mut handles = vec![];
    for i in 0..16 {
        let reg = registry.clone();
        let nullifier = [(i % 8) as u8; 32];
        handles.push(thread::spawn(move || reg.mark_many_atomic(&[nullifier])));
    }
    let mut successes = 0;
    let mut failures = 0;
    for h in handles {
        match h.join().unwrap() {
            Ok(()) => successes += 1,
            Err(_) => failures += 1,
        }
    }
    assert_eq!(successes, 8);
    assert_eq!(failures, 8);
    assert_eq!(registry.len(), 8);
}
