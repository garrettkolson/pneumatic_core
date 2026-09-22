use super::helpers::*;
use super::super::*;
use std::sync::{Arc, Mutex};


// --- Gas deduction tests ---

#[tokio::test]
async fn check_and_commit_deducts_gas_from_user() {
    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 1000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer(dp.clone());

    // Bootstrap token and chain BEFORE creating block
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_gas_deduct";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
    registry.record_gas_used(tx_id, 50);

    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    if let Err(ref e) = result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok());

    let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
    assert_eq!(user.fuel_balance, 950);
}


#[tokio::test]
async fn check_and_commit_no_gas_tracked_does_not_deduct() {
    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"bob".to_vec(), "token".to_string(), User {
        public_key: b"bob".to_vec(),
        fuel_balance: 500,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer(dp.clone());

    // Bootstrap token and chain BEFORE creating block
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_no_gas";
    make_finalizing_entry(&registry, tx_id, b"bob".to_vec());
    // No record_gas_used called

    let block = make_test_block_for_token(&committer, tx_id, b"bob".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(result.is_ok());

    let user = dp.get_user(&b"bob".to_vec(), "token").unwrap();
    assert_eq!(user.fuel_balance, 500);
}


#[tokio::test]
async fn check_and_commit_gas_exceeds_balance_saturates() {
    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"charlie".to_vec(), "token".to_string(), User {
        public_key: b"charlie".to_vec(),
        fuel_balance: 100,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer(dp.clone());

    // Bootstrap token and chain BEFORE creating block
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_sat";
    make_finalizing_entry(&registry, tx_id, b"charlie".to_vec());
    registry.record_gas_used(tx_id, 200);

    let block = make_test_block_for_token(&committer, tx_id, b"charlie".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(result.is_ok());

    let user = dp.get_user(&b"charlie".to_vec(), "token").unwrap();
    assert_eq!(user.fuel_balance, 0);
}


#[tokio::test]
async fn save_user_failure_is_reported_not_swallowed() {
    let dp = Arc::new(TestDataProvider::with_failures(false, true));
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 1000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer(dp.clone());

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_save_fail";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
    registry.record_gas_used(tx_id, 50);

    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    // M11: a failed `save_user` must surface as `GasDeduction` — the old `let _ =` swallowed
    // it, letting the tx reach Committed with no debit. The committed block stands (it was
    // validated/finalized); the failure is returned, not dropped.
    let result = committer.check_and_commit_transaction_results(&commit, vec![3]).await;
    assert_gas_deduction(result, b"alice", tx_id, 50);

    // The sender was never debited, so the tx must stay Finalizing (observable as a failed
    // settlement), NOT Committed.
    let entry = registry.get_transaction_mut(tx_id).unwrap();
    assert!(
        matches!(entry.state, TransactionState::Finalizing { .. }),
        "tx must stay Finalizing after a failed deduction, got {:?}",
        entry.state
    );

    // No gas given for free — balance unchanged from the pre-commit value.
    let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
    assert_eq!(user.fuel_balance, 1000);
}


#[tokio::test]
async fn get_user_failure_is_reported_not_swallowed() {
    let dp = Arc::new(TestDataProvider::with_failures(true, false));
    dp.insert_user(b"bob".to_vec(), "token".to_string(), User {
        public_key: b"bob".to_vec(),
        fuel_balance: 1000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer(dp.clone());

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_get_fail";
    make_finalizing_entry(&registry, tx_id, b"bob".to_vec());
    registry.record_gas_used(tx_id, 50);

    let block = make_test_block_for_token(&committer, tx_id, b"bob".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    // M11: a failed `get_user` (blocked data service) must surface too, not be silently
    // skipped (the old `if let Ok(mut user)` skipped the whole deduction).
    let result = committer.check_and_commit_transaction_results(&commit, vec![3]).await;
    assert_gas_deduction(result, b"bob", tx_id, 50);

    let entry = registry.get_transaction_mut(tx_id).unwrap();
    assert!(
        matches!(entry.state, TransactionState::Finalizing { .. }),
        "tx must stay Finalizing after a failed deduction, got {:?}",
        entry.state
    );

    // get_user is deliberately failing here, so read the stored value directly to confirm the
    // balance was never touched — gas was neither deducted nor, as the fail-closed rule requires,
    // silently freed (the tx is not Committed either).
    assert_eq!(dp.raw_balance(&b"bob".to_vec(), "token"), Some(1000));
}


#[tokio::test]
async fn gas_deduction_failure_logs() {
    let dp = Arc::new(TestDataProvider::with_failures(false, true));
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 1000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, collector) = make_test_committer(dp.clone());

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_log";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
    registry.record_gas_used(tx_id, 50);

    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![3]).await;
    assert!(matches!(result, Err(CommitterError::GasDeduction { .. })));

    let logs = collector.logs.lock().unwrap();
    let hit = logs
        .iter()
        .find(|l| l.contains("GAS DEDUCTION FAILED"))
        .expect("expected an observable 'GAS DEDUCTION FAILED' log line");
    // The failure line carries the sender (hex), tx id, gas used, and the error cause.
    assert!(hit.contains(&bytes_to_hex(b"alice")), "log was: {hit}");
    assert!(hit.contains(tx_id), "log was: {hit}");
    assert!(hit.contains("50"), "log was: {hit}");
    assert!(hit.contains("StoreNotFound"), "log was: {hit}");
}


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_sender_commits_both_deduct() {
    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 1000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer(dp.clone());

    // Two independent tokens, each with its own genesis-seeded, self-verified chain, let both
    // commits run concurrently without colliding on block linkage (they share only the sender).
    // Marking self_verified is required: fail-closed block validation (AUDIT Phase 3.2 / C5)
    // only accepts a process-style block on a chain whose token is self_verified.
    for id in [vec![1], vec![2]] {
        let mut token = Token::new();
        token.id = id.clone();
        token.is_self_verified = true;
        committer.bootstrap_token(token);

        let mut genesis = Block {
            signed_trans: SignedTransaction::test_transaction(),
            token_metadata: HashMap::new(),
            previous_hash: vec![42u8; 32],
            current_hash: vec![],
            timestamp: 0,
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        let genesis_hash = pneumatic_core::blocks::BlockFactory::create_hash(&genesis)
            .expect("well-formed test block hashes");
        if let Some(mut entry) = committer.tokens.get_mut(&id) {
            genesis.current_hash = genesis_hash;
            entry.value_mut().blockchain.add_block(genesis);
        }
    }

    for tx_id in ["tx_conc_1", "tx_conc_2"] {
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
        registry.record_gas_used(tx_id, 50);
    }

    let block1 = make_block_for_token_id(&committer, "tx_conc_1", b"alice".to_vec(), &vec![1]);
    let commit1 = TransactionCommit {
        trans_id: b"tx_conc_1".to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block1,
    };
    let block2 = make_block_for_token_id(&committer, "tx_conc_2", b"alice".to_vec(), &vec![2]);
    let commit2 = TransactionCommit {
        trans_id: b"tx_conc_2".to_vec(),
        token_id: vec![2],
        env_id: "test".to_string(),
        proposed_block: block2,
    };

    // Fire both commits for the SAME sender concurrently on a two-worker runtime, so the
    // get_user -> subtract -> save_user read-modify-write can genuinely race.
    let f1 = committer.check_and_commit_transaction_results(&commit1, vec![3]);
    let f2 = committer.check_and_commit_transaction_results(&commit2, vec![3]);
    let (r1, r2) = tokio::join!(f1, f2);
    assert!(r1.is_ok(), "commit1 failed: {r1:?}");
    assert!(r2.is_ok(), "commit2 failed: {r2:?}");

    // With the per-sender RMW lock the two 50-unit deductions serialize: 1000 -> 900.
    // Without the lock, last-write-wins loses one deduction (balance would be 950), which
    // is the exact lost-update M11 guards against.
    let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
    assert_eq!(user.fuel_balance, 900);
}
