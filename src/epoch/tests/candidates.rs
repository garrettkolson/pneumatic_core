//! CandidateRegistry tests: creation, LRU eviction, insert/get, conflict
//! detection, removal, key independence, and the concurrent races.
use super::helpers::*;
use super::super::*;

// --- CandidateRegistry tests ---

#[test]
fn registry_empty_on_creation() {
    let registry = CandidateRegistry::new();
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
}

// --- CandidateRegistry LRU bound (AUDIT Phase 5.2 / H2) ---

#[test]
fn registry_lru_evicts_oldest_when_over_cap() {
    // A per-position candidate vec is capped at `max_candidates`; the oldest
    // candidate is evicted once the cap is exceeded, so repeated conflicting
    // proposals cannot inflate the registry.
    let registry = CandidateRegistry::with_max_candidates(3);
    let token_id = vec![7];
    let prev_hash = vec![8];

    // Insert 5 candidates (cap is 3); each carries a distinguishable
    // proposer key so we can confirm the oldest is dropped.
    let first = vec![1];
    for proposer in [first, vec![2], vec![3], vec![4], vec![5]] {
        let block = Block::test_block(proposer.clone());
        registry.insert(token_id.clone(), prev_hash.clone(), block, proposer);
    }

    let candidates = registry.get_candidates(&token_id, &prev_hash);
    assert_eq!(candidates.len(), 3, "per-position count stays capped at max");
    // Oldest (proposer [1]) evicted; the three most recent remain, in order.
    let proposers: Vec<Vec<u8>> = candidates.iter().map(|(_, pk)| pk.clone()).collect();
    assert_eq!(proposers, vec![vec![3], vec![4], vec![5]]);
}

#[test]
fn registry_insert_and_get_candidates() {
    let registry = CandidateRegistry::new();
    let block = Block::test_block(vec![1, 2, 3]);
    let token_id = vec![1, 2];
    let prev_hash = vec![4, 5, 6];
    let proposer = vec![99];

    registry.insert(token_id.clone(), prev_hash.clone(), block, proposer.clone());

    let candidates = registry.get_candidates(&token_id, &prev_hash);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].1, proposer);
}

#[test]
fn registry_detects_conflict_on_second_insert() {
    let registry = CandidateRegistry::new();
    let block_a = Block::test_block(vec![1]);
    let block_b = Block::test_block(vec![2]);
    let token_id = vec![1, 2];
    let prev_hash = vec![3, 4, 5];

    registry.insert(token_id.clone(), prev_hash.clone(), block_a, vec![1]);
    assert!(!registry.has_conflict(&token_id, &prev_hash));

    registry.insert(token_id.clone(), prev_hash.clone(), block_b, vec![2]);
    assert!(registry.has_conflict(&token_id, &prev_hash));
}

#[test]
fn registry_candidate_count_returns_correct_count() {
    let registry = CandidateRegistry::new();
    let token_id = vec![1];
    let prev_hash = vec![2];

    assert_eq!(registry.candidate_count(&token_id, &prev_hash), 0);

    registry.insert(token_id.clone(), prev_hash.clone(), Block::test_block(vec![1]), vec![1]);
    assert_eq!(registry.candidate_count(&token_id, &prev_hash), 1);

    registry.insert(token_id.clone(), prev_hash.clone(), Block::test_block(vec![2]), vec![2]);
    assert_eq!(registry.candidate_count(&token_id, &prev_hash), 2);
}

#[test]
fn registry_remove_conflicted_clears_entry() {
    let registry = CandidateRegistry::new();
    let token_id = vec![1, 2];
    let prev_hash = vec![3, 4, 5];

    registry.insert(token_id.clone(), prev_hash.clone(), Block::test_block(vec![1]), vec![1]);
    registry.insert(token_id.clone(), prev_hash.clone(), Block::test_block(vec![2]), vec![2]);

    assert_eq!(registry.candidate_count(&token_id, &prev_hash), 2);
    let removed = registry.remove_conflicted(&token_id, &prev_hash);
    assert_eq!(removed, 2);
    assert!(registry.is_empty());
}

#[test]
fn registry_separate_keys_independent() {
    let registry = CandidateRegistry::new();
    let block = Block::test_block(vec![1]);

    registry.insert(vec![1], vec![2], block.clone(), vec![1]);
    registry.insert(vec![3], vec![4], block, vec![2]);

    assert_eq!(registry.len(), 2);
    assert_eq!(registry.candidate_count(&[1], &[2]), 1);
    assert_eq!(registry.candidate_count(&[3], &[4]), 1);
    assert!(!registry.has_conflict(&[1], &[2]));
}

// --- CandidateRegistry concurrent tests ---

#[test]
fn registry_concurrent_inserts_no_panic() {
    let registry = Arc::new(CandidateRegistry::new());
    let token_id = vec![1];
    let prev_hash = vec![2];
    let mut handles = vec![];

    for i in 0..10 {
        let reg = Arc::clone(&registry);
        let tid = token_id.clone();
        let ph = prev_hash.clone();
        handles.push(std::thread::spawn(move || {
            let block = Block::test_block(vec![i as u8]);
            reg.insert(tid, ph, block, vec![i]);
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    assert_eq!(registry.candidate_count(&token_id, &prev_hash), 10);
    assert!(registry.has_conflict(&token_id, &prev_hash));
}

#[test]
fn registry_concurrent_separate_keys_no_race() {
    let registry = Arc::new(CandidateRegistry::new());
    let mut handles = vec![];

    for i in 0..5 {
        let reg = Arc::clone(&registry);
        handles.push(std::thread::spawn(move || {
            let token_id = vec![i];
            let prev_hash = vec![i, 0];
            let block = Block::test_block(vec![i as u8]);
            reg.insert(token_id, prev_hash, block, vec![i]);
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    assert_eq!(registry.len(), 5);
}
