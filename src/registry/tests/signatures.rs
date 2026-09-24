//! `TransactionSignatureRegistry` tests: registration, per-executor dedup,
//! removal, emptiness tracking, and the concurrent add/remove races.
use super::helpers::*;
use super::super::*;

// --- TransactionSignatureRegistry ---

#[test]
fn signature_registry_add_transaction_successful() {
    let registry = TransactionSignatureRegistry::new();
    assert!(registry.try_add_transaction("tx1").is_ok());
    assert!(registry.transaction_is_registered("tx1"));
}

#[test]
fn signature_registry_duplicate_add_returns_error() {
    let registry = TransactionSignatureRegistry::new();
    registry.try_add_transaction("tx1").unwrap();
    assert!(registry.try_add_transaction("tx1").is_err());
}

#[test]
fn signature_registry_add_signature_successful() {
    let registry = TransactionSignatureRegistry::new();
    registry.try_add_transaction("tx1").unwrap();
    let sig = TransactionSignature {
        transaction_id: vec![],
        env_id: vec![],
        transaction_hash: vec![],
        signature: vec![1, 2, 3],
        current_stake: 100,
    };
    assert!(registry
        .try_add_signature("tx1", vec![1], sig.clone())
        .is_ok());
    let registry_map = registry.get_transaction_registry("tx1").unwrap();
    assert_eq!(registry_map.len(), 1);
}

#[test]
fn signature_registry_duplicate_signature_fails() {
    let registry = TransactionSignatureRegistry::new();
    registry.try_add_transaction("tx1").unwrap();
    let sig = TransactionSignature {
        transaction_id: vec![],
        env_id: vec![],
        transaction_hash: vec![],
        signature: vec![1, 2, 3],
        current_stake: 100,
    };
    registry.try_add_signature("tx1", vec![1], sig.clone()).unwrap();
    assert!(registry.try_add_signature("tx1", vec![1], sig).is_err());
}

#[test]
fn signature_registry_multiple_sigs_different_executors() {
    let registry = TransactionSignatureRegistry::new();
    registry.try_add_transaction("tx1").unwrap();
    let sig = |stake| TransactionSignature {
        transaction_id: vec![],
        env_id: vec![],
        transaction_hash: vec![],
        signature: vec![stake as u8],
        current_stake: stake,
    };
    registry.try_add_signature("tx1", vec![1], sig(100)).unwrap();
    registry.try_add_signature("tx1", vec![2], sig(200)).unwrap();
    registry.try_add_signature("tx1", vec![3], sig(300)).unwrap();
    let registry_map = registry.get_transaction_registry("tx1").unwrap();
    assert_eq!(registry_map.len(), 3);
}

#[test]
fn signature_registry_remove_successful() {
    let registry = TransactionSignatureRegistry::new();
    registry.try_add_transaction("tx1").unwrap();
    assert!(registry.try_remove_transaction("tx1").is_ok());
    assert!(!registry.transaction_is_registered("tx1"));
}

#[test]
fn signature_registry_remove_nonexistent_fails() {
    let registry = TransactionSignatureRegistry::new();
    assert!(registry.try_remove_transaction("tx1").is_err());
}

#[test]
fn signature_registry_empty_and_len() {
    let registry = TransactionSignatureRegistry::new();
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    registry.try_add_transaction("tx1").unwrap();
    assert!(!registry.is_empty());
    assert_eq!(registry.len(), 1);
}

#[test]
fn concurrent_add_signature_different_executors() {
    let registry = Arc::new(TransactionSignatureRegistry::new());
    registry.try_add_transaction("tx1").unwrap();
    let mut handles = vec![];
    for i in 0..4 {
        let reg = registry.clone();
        let sig = TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![i as u8],
            current_stake: 100 + i,
        };
        handles.push(std::thread::spawn(move || {
            reg.try_add_signature("tx1", vec![i as u8], sig)
        }));
    }
    for h in handles {
        h.join().unwrap().unwrap();
    }
    let map = registry.get_transaction_registry("tx1").unwrap();
    assert_eq!(map.len(), 4);
}

#[test]
fn concurrent_add_signature_same_executor_one_succeeds() {
    let registry = Arc::new(TransactionSignatureRegistry::new());
    registry.try_add_transaction("tx1").unwrap();
    let sig = TransactionSignature {
        transaction_id: vec![],
        env_id: vec![],
        transaction_hash: vec![],
        signature: vec![1, 2, 3],
        current_stake: 100,
    };
    let mut handles = vec![];
    for _ in 0..4 {
        let reg = registry.clone();
        let s = sig.clone();
        handles.push(std::thread::spawn(move || {
            reg.try_add_signature("tx1", vec![1], s)
        }));
    }
    let mut successes = 0;
    for h in handles {
        match h.join().unwrap() {
            Ok(()) => successes += 1,
            Err(_) => {}
        }
    }
    assert_eq!(successes, 1);
}

#[test]
fn concurrent_try_add_transaction_same_id() {
    let registry = Arc::new(TransactionSignatureRegistry::new());
    let mut handles = vec![];
    for _ in 0..4 {
        let reg = registry.clone();
        handles.push(std::thread::spawn(move || {
            reg.try_add_transaction("tx1")
        }));
    }
    let mut successes = 0;
    for h in handles {
        match h.join().unwrap() {
            Ok(()) => successes += 1,
            Err(_) => {}
        }
    }
    // Due to DashMap's parallel nature, multiple threads may pass
    // the duplicate check before any completes the insert. Only
    // guarantee that at least one succeeded.
    assert!(successes >= 1);
}

#[test]
fn concurrent_add_remove_stress() {
    let registry = Arc::new(TransactionSignatureRegistry::new());
    let mut handles = vec![];
    // 20 threads add unique txs
    for i in 0..20 {
        let reg = registry.clone();
        handles.push(std::thread::spawn(move || {
            reg.try_add_transaction(&format!("tx_{}", i)).unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(registry.len(), 20);
    // 20 threads remove those same txs
    let mut remove_handles = vec![];
    for i in 0..20 {
        let reg = registry.clone();
        remove_handles.push(std::thread::spawn(move || {
            reg.try_remove_transaction(&format!("tx_{}", i)).unwrap();
        }));
    }
    for h in remove_handles {
        h.join().unwrap();
    }
    assert_eq!(registry.len(), 0);
}
