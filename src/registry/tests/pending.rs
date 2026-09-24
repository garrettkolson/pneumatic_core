//! `PendingTransactionRegistry` tests: register/add/remove/acquire/release
//! lifecycle, validation-result reads, requested-finalizer marking,
//! shielded round-trips, the gas tracker, and the concurrent stress cases.
use super::helpers::*;
use super::super::*;

// --- PendingTransactionRegistry ---

#[test]
fn contains_empty_registry_returns_false() {
    let registry = PendingTransactionRegistry::new();
    assert!(!registry.contains("tx1"));
}

#[test]
fn contains_after_register_pending_returns_true() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    assert!(registry.contains("tx1"));
}

#[test]
fn register_pending_duplicate_returns_error() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    assert!(registry.register_pending("tx1".into()).is_err());
}

#[test]
fn enqueue_to_pool_rejects_duplicate_nonce() {
    // Phase 5.6 / H14: a reused (token_id, sender, sequence_number) is a replay.
    let registry = PendingTransactionRegistry::new();
    let token_id = vec![1, 2, 3];
    let sender = vec![9, 9, 9];

    // First admission of (token_id, sender, seq=5) succeeds.
    assert!(registry
        .enqueue_to_pool("tx1", token_id.clone(), 5, 100, sender.clone())
        .is_ok());

    // A different tx_id carrying the SAME (token_id, sender, seq=5) is a replayed nonce.
    assert!(registry
        .enqueue_to_pool("tx2", token_id.clone(), 5, 101, sender.clone())
        .is_err());

    // A distinct sequence number for the same sender is not a replay.
    assert!(registry
        .enqueue_to_pool("tx3", token_id.clone(), 6, 102, sender.clone())
        .is_ok());
}

#[test]
fn add_transaction_creates_pending_state() {
    let registry = PendingTransactionRegistry::new();
    let tx = PendingTransaction::new("tx1".into(), TransactionState::Pending);
    registry.add_transaction("tx1".into(), tx).unwrap();
    let entry = registry.get_transaction_mut("tx1").unwrap();
    assert!(matches!(entry.state, TransactionState::Pending));
}

#[test]
fn add_transaction_duplicate_returns_error() {
    let registry = PendingTransactionRegistry::new();
    let tx = PendingTransaction::new("tx1".into(), TransactionState::Pending);
    registry.add_transaction("tx1".into(), tx).unwrap();
    let tx2 = PendingTransaction::new("tx1".into(), TransactionState::Pending);
    assert!(registry.add_transaction("tx1".into(), tx2).is_err());
}

#[test]
fn remove_transaction_successful() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    assert!(registry.remove_transaction("tx1").is_ok());
    assert!(!registry.contains("tx1"));
}

#[test]
fn remove_nonexistent_returns_error() {
    let registry = PendingTransactionRegistry::new();
    assert!(registry.remove_transaction("tx1").is_err());
}

#[test]
fn acquire_transaction_found_succeeds() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    assert!(registry.acquire_transaction("tx1").is_ok());
}

#[test]
fn acquire_nonexistent_returns_error() {
    let registry = PendingTransactionRegistry::new();
    assert!(registry.acquire_transaction("tx1").is_err());
}

#[test]
fn acquire_terminal_state_fails() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    registry.acquire_transaction("tx1").unwrap();
    // Transition to Failed
    if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
        entry.transition_to_failed(
            Transaction {
                id: "tx1".into(), action: "Transfer".into(),
                token_id: vec![], bid: None, sequence_number: 0,
                sender: vec![], receiver: vec![], amount: None,
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
            },
            vec![],
        );
    }
    assert!(registry.acquire_transaction("tx1").is_err());
}

#[test]
fn get_validation_result_from_validated_returns_some() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    registry.acquire_transaction("tx1").unwrap();
    // Transition to Validated
    if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
        entry.transition_to_validated(
            Transaction {
                id: "tx1".into(), action: "Transfer".into(),
                token_id: vec![], bid: None, sequence_number: 1,
                sender: vec![1], receiver: vec![2], amount: Some(100),
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
            },
            TransactionValidationResult::valid(
                vec![1],
                TransactionRiskFactor {
                    affected_parties: 2, amount: 100,
                    is_contract: false, is_multi_party: false,
                },
            ),
        );
    }
    let result = registry.get_validation_result("tx1").unwrap();
    assert!(result.is_valid);
}

#[test]
fn get_validation_result_from_pending_returns_error() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    assert!(registry.get_validation_result("tx1").is_err());
}

#[test]
fn release_transaction_successful_keeps_pending() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    registry.acquire_transaction("tx1").unwrap();
    let result = registry.release_transaction("tx1").unwrap();
    assert!(!result); // Pending, not terminal → false
    assert!(registry.contains("tx1")); // still in registry
}

#[test]
fn release_failed_transaction_returns_true() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
        entry.transition_to_failed(
            Transaction {
                id: "tx1".into(), action: "Transfer".into(),
                token_id: vec![], bid: None, sequence_number: 0,
                sender: vec![], receiver: vec![], amount: None,
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
            },
            vec![ValidationFailureReason::InsufficientFunds],
        );
    }
    let result = registry.release_transaction("tx1").unwrap();
    assert!(result); // Failed, lock=0 → true (caller should remove)
    // Note: release_transaction returns true to signal removal but doesn't remove itself
    assert!(registry.contains("tx1")); // still in registry until removed
}

#[test]
fn set_requested_finalizer_validated_succeeds() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    // Transition to Validated first
    if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
        entry.transition_to_validated(
            Transaction {
                id: "tx1".into(), action: "Transfer".into(),
                token_id: vec![], bid: None, sequence_number: 1,
                sender: vec![1], receiver: vec![2], amount: Some(100),
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
            },
            TransactionValidationResult::valid(vec![], TransactionRiskFactor { affected_parties: 1, amount: 0, is_contract: false, is_multi_party: false }),
        );
    }
    assert!(registry.set_requested_finalizer("tx1", vec![99]).is_ok());
}

#[test]
fn set_requested_finalizer_from_pending_fails() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    assert!(registry.set_requested_finalizer("tx1", vec![99]).is_err());
}

#[test]
fn is_requested_finalizer_matches() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
        entry.transition_to_validated(
            Transaction {
                id: "tx1".into(), action: "Transfer".into(),
                token_id: vec![], bid: None, sequence_number: 1,
                sender: vec![1], receiver: vec![2], amount: Some(100),
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
            },
            TransactionValidationResult::valid(vec![], TransactionRiskFactor { affected_parties: 1, amount: 0, is_contract: false, is_multi_party: false }),
        );
    }
    registry.set_requested_finalizer("tx1", vec![99]).unwrap();
    assert!(registry.is_requested_finalizer("tx1", &[99]));
}

#[test]
fn is_requested_finalizer_mismatch() {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
        entry.transition_to_validated(
            Transaction {
                id: "tx1".into(), action: "Transfer".into(),
                token_id: vec![], bid: None, sequence_number: 1,
                sender: vec![1], receiver: vec![2], amount: Some(100),
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
            },
            TransactionValidationResult::valid(vec![], TransactionRiskFactor { affected_parties: 1, amount: 0, is_contract: false, is_multi_party: false }),
        );
    }
    registry.set_requested_finalizer("tx1", vec![99]).unwrap();
    assert!(!registry.is_requested_finalizer("tx1", &[1, 2, 3]));
}

/// Admit → contains → get round-trips the exact tx (the clone is the
/// canonical copy; no field is lost in flight).
#[test]
fn register_shielded_roundtrips_through_map() {
    let registry = PendingTransactionRegistry::new();
    let tx = shielded_fixture("shd_1");

    assert!(!registry.contains_shielded("shd_1"), "fresh registry: absent");
    assert!(
        registry.get_shielded("shd_1").is_none(),
        "fresh registry: no entry to fetch"
    );

    registry.register_shielded(&tx).unwrap();

    assert!(registry.contains_shielded("shd_1"), "admitted: present");
    assert_eq!(
        registry.get_shielded("shd_1"),
        Some(tx.clone()),
        "fetched entry equals the admitted tx exactly"
    );
}

/// A duplicate id is rejected atomically — `insert`'s returned old value
/// is the race-free duplicate check — and the rejection leaves the
/// original entry untouched (a silent overwrite would let a second,
/// differently-proven tx squat a signed id).
#[test]
fn register_shielded_duplicate_id_rejected_atomically() {
    let registry = PendingTransactionRegistry::new();
    let first = shielded_fixture("shd_dup");

    registry.register_shielded(&first).unwrap();

    // Same id, *different* proof: the duplicate is a protocol violation.
    let mut impostor = first.clone();
    impostor.proof = vec![0xDE, 0xAD];
    let err = registry.register_shielded(&impostor).unwrap_err();
    assert!(
        matches!(err, PneumaticError::Registry(_)),
        "duplicate id is a Registry error, got: {:?}",
        err
    );

    // The original is intact — nothing was overwritten.
    assert_eq!(
        registry.get_shielded("shd_dup"),
        Some(first),
        "the first-admitted entry survives a duplicate rejection"
    );
}

// --- Gas tracker tests ---

#[test]
fn gas_tracker_records_and_retrieves() {
    let registry = PendingTransactionRegistry::new();
    registry.record_gas_used("tx1", 42);
    assert_eq!(registry.get_gas_used("tx1"), Some(42));
}

#[test]
fn gas_tracker_returns_none_for_unknown_tx() {
    let registry = PendingTransactionRegistry::new();
    assert_eq!(registry.get_gas_used("nonexistent"), None);
}

// --- Concurrent tests ---

#[test]
fn concurrent_register_pending_same_id() {
    let registry = Arc::new(PendingTransactionRegistry::new());
    let mut handles = vec![];
    for _ in 0..2 {
        let reg = registry.clone();
        handles.push(std::thread::spawn(move || {
            reg.register_pending("tx1".into())
        }));
    }
    let mut successes = 0;
    let mut failures = 0;
    for h in handles {
        match h.join().unwrap() {
            Ok(()) => successes += 1,
            Err(_) => failures += 1,
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(failures, 1);
}

#[test]
fn concurrent_register_pending_different_ids_all_succeed() {
    let registry = Arc::new(PendingTransactionRegistry::new());
    let mut handles = vec![];
    for i in 0..10 {
        let reg = registry.clone();
        handles.push(std::thread::spawn(move || {
            reg.register_pending(format!("tx_{}", i))
        }));
    }
    for h in handles {
        h.join().unwrap().unwrap();
    }
    for i in 0..10 {
        assert!(registry.contains(&format!("tx_{}", i)));
    }
}

#[test]
fn concurrent_acquire_release_same_entry() {
    let registry = Arc::new(PendingTransactionRegistry::new());
    registry.register_pending("tx1".into()).unwrap();
    let mut acq_handles = vec![];
    // 4 threads acquire
    for _ in 0..4 {
        let reg = registry.clone();
        acq_handles.push(std::thread::spawn(move || {
            let _ = reg.acquire_transaction("tx1");
        }));
    }
    for h in acq_handles {
        h.join().unwrap();
    }
    // 4 threads release
    let mut rel_handles = vec![];
    for _ in 0..4 {
        let reg = registry.clone();
        rel_handles.push(std::thread::spawn(move || {
            let _ = reg.release_transaction("tx1");
        }));
    }
    for h in rel_handles {
        h.join().unwrap();
    }
    // Registry may or may not contain tx depending on race conditions — just check no panic
    let _ = registry.contains("tx1");
}

#[test]
fn concurrent_acquire_terminal_state_rejected() {
    let registry = Arc::new(PendingTransactionRegistry::new());
    registry.register_pending("tx1".into()).unwrap();
    // Transition one entry to Failed
    {
        let mut entry = registry.transactions.get_mut("tx1").unwrap();
        entry.transition_to_failed(
            Transaction {
                id: "tx1".into(), action: "Transfer".into(),
                token_id: vec![], bid: None, sequence_number: 0,
                sender: vec![], receiver: vec![], amount: None,
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
            },
            vec![],
        );
    }
    let mut handles = vec![];
    for _ in 0..4 {
        let reg = registry.clone();
        handles.push(std::thread::spawn(move || {
            reg.acquire_transaction("tx1")
        }));
    }
    for h in handles {
        assert!(h.join().unwrap().is_err());
    }
}

#[test]
fn concurrent_remove_during_acquire() {
    let registry = Arc::new(PendingTransactionRegistry::new());
    registry.register_pending("tx1".into()).unwrap();
    let remove_handle = {
        let reg = registry.clone();
        std::thread::spawn(move || reg.remove_transaction("tx1"))
    };
    std::thread::sleep(std::time::Duration::from_millis(5));
    let acquire_handle = {
        let reg = registry.clone();
        std::thread::spawn(move || reg.acquire_transaction("tx1"))
    };
    let _ = remove_handle.join().unwrap();
    match acquire_handle.join().unwrap() {
        Ok(()) => panic!("should have failed — tx was removed"),
        Err(_) => {} // expected
    }
}

#[test]
fn concurrent_acquire_release_stress_50() {
    let registry = Arc::new(PendingTransactionRegistry::new());
    registry.register_pending("tx1".into()).unwrap();
    let mut handles = vec![];
    for _ in 0..50 {
        let reg = registry.clone();
        handles.push(std::thread::spawn(move || {
            let _ = reg.acquire_transaction("tx1");
            std::thread::sleep(std::time::Duration::from_millis(1));
            let _ = reg.release_transaction("tx1");
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // No panics — that's the point
}

#[test]
fn concurrent_release_zero_pending_keeps() {
    let registry = Arc::new(PendingTransactionRegistry::new());
    registry.register_pending("tx1".into()).unwrap();
    let mut handles = vec![];
    for _ in 0..4 {
        let reg = registry.clone();
        handles.push(std::thread::spawn(move || {
            reg.release_transaction("tx1").unwrap()
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // Pending with lock=0 → release returns false, tx stays
    assert!(registry.contains("tx1"));
}
