use super::helpers::*;
use super::super::*;
use std::sync::{Arc, Mutex};


#[tokio::test]
async fn check_and_commit_validated_state_succeeds() {
    // Leader-proposal path: transaction is in Validated state (not Finalizing).
    // The Committer should still commit the block and transition to Committed.
    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 1000,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer(dp.clone());

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_leader_proposal";
    make_validated_entry(&registry, tx_id, b"alice".to_vec());
    registry.record_gas_used(tx_id, 75);

    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(result.is_ok());

    // Gas was deducted
    let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
    assert_eq!(user.fuel_balance, 925);

    // Transaction was removed from pool (leader-proposal path)
    assert!(!registry.contains(tx_id));
}


#[tokio::test]
async fn check_and_commit_validated_saturates_on_overflow() {
    // Leader-proposal path with gas exceeding balance — should saturate to 0
    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"bob".to_vec(), "token".to_string(), User {
        public_key: b"bob".to_vec(),
        fuel_balance: 50,
        stake: 0,
        nonce: 0,
    });
    let (committer, registry, _logger) = make_test_committer(dp.clone());

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_leader_overflow";
    make_validated_entry(&registry, tx_id, b"bob".to_vec());
    registry.record_gas_used(tx_id, 200); // exceeds balance

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
    assert_eq!(user.fuel_balance, 0);
}


// --- Payload-match + block_hash registry (AUDIT Phase 3.5 / H12) ---

#[tokio::test]
async fn check_and_commit_rejects_payload_mismatch() {
    // Headline H12 discriminator. A commit whose block embeds a transaction differing from the
    // validated/pooled one must be rejected — without the payload-match gate the Committer would
    // happily append whatever block arrived on the wire.
    let dp = Arc::new(TestDataProvider::new());
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

    let tx_id = "tx_payload_mismatch";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

    // Build a matching block, then tamper its embedded transaction (swap receiver) so the wire
    // payload differs from the validated entry the Committer holds.
    let mut block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    block.signed_trans.transaction.receiver = vec![255];

    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    match committer.check_and_commit_transaction_results(&commit, vec![]).await {
        Err(CommitterError::TransactionPayloadMismatch(_)) => {}
        other => panic!("expected TransactionPayloadMismatch, got {other:?}"),
    }
}


#[tokio::test]
async fn committed_transaction_records_block_hash_not_token_id() {
    // Misnomer discriminator (H12). Committed.block_hash must record the hash of the block the
    // transaction was committed *into*, never the token id (which is what the pre-fix Committer
    // stored). We pin the entry with a second lock so it survives the commit flow (which would
    // otherwise remove it once lock_count hits 0) and inspect its persisted state.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp.clone());

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_blockhash";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
    {
        let mut entry = registry.get_transaction_mut(tx_id).unwrap();
        entry.acquire().unwrap();
    }

    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let committed_hash = block.current_hash.clone();
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    assert!(committer.check_and_commit_transaction_results(&commit, vec![]).await.is_ok());

    // The entry persists (pinned by the second lock); read back its Committed.block_hash.
    let entry = registry.get_transaction_mut(tx_id).unwrap();
    match &entry.state {
        TransactionState::Committed { transaction, block_hash } => {
            assert_eq!(block_hash, &committed_hash);
            assert_ne!(block_hash, &vec![1]); // not the token id
            assert_eq!(&transaction.sender, &b"alice".to_vec());
        }
        other => panic!("expected Committed, got {other:?}"),
    }
}
