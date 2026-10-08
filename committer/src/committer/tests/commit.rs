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
    assert!(result.is_ok(), "{result:?}");

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

// --- Phase 1 (ingress roadmap): committed-chain persistence + cold-cache warm ---

/// The committed chain reaches the data service. Before this was pinned, a
/// normal (non-deploy) commit mutated only the committer's in-memory cache:
/// no `save_token` ever ran, so the committed block was not observable through
/// the data service, the ADR-019 selection salt for the token stayed pinned at
/// its genesis tip forever, and a committer restart silently rolled the chain
/// back. This test reads the advanced chain back through the provider read
/// path — the same read every other role performs.
#[tokio::test]
async fn committed_block_persists_the_advanced_token_to_the_data_service() {
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
    let genesis_tip = {
        let entry = committer.tokens.get(&vec![1]).expect("bootstrapped token");
        entry.value().blockchain.get_current_chain_state().last_hash_in.clone()
    };

    let tx_id = "tx_persist";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let committed_hash = block.current_hash.clone();
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    assert!(committer.check_and_commit_transaction_results(&commit, vec![]).await.is_ok());

    // The provider (data service) copy — not the cache copy — must carry the
    // appended block. This is the exact read an outside observer (a client,
    // the Phase 1 exit test, a restarting node) performs.
    let stored = dp
        .get_token(&vec![1], "token")
        .expect("the committed chain must be readable through the data service");
    assert_eq!(stored.blockchain.chain.len(), 2, "the committed block must be ON the stored chain");
    assert_eq!(
        stored.blockchain.get_current_chain_state().last_hash_in, committed_hash,
        "the stored tip must be the committed block, not the genesis tip it was before"
    );
    assert_ne!(committed_hash, genesis_tip, "the fixture genuinely extends the chain");
}

/// A cold committer — empty token cache, token present only in the data
/// service — must commit by warming its cache from the provider. Before the
/// Phase 1 fix, `commit_block` was cache-only: a freshly-booted committer
/// failed every Commit against a genesis-seeded token with `TokenNotFound`,
/// because nothing populated the cache for a token that was not created by a
/// committed `DeployContract` (`distribute_token` has no production caller).
#[tokio::test]
async fn cold_cache_commit_warms_the_token_from_the_data_service() {
    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
        public_key: b"alice".to_vec(),
        fuel_balance: 1000,
        stake: 0,
        nonce: 0,
    });

    // The token exists ONLY in the provider: genesis-chained, self-verified
    // (the same block-spec posture `bootstrap_token_chain` pins for this suite).
    let genesis = {
        let signed = SignedTransaction::test_transaction();
        let mut genesis = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: vec![42u8; 32],
            timestamp: 0,
            current_hash: vec![],
            finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        genesis.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&genesis)
            .expect("well-formed genesis hashes");
        genesis
    };
    let genesis_tip = genesis.current_hash.clone();
    {
        let mut token = Token::new();
        token.id = vec![1];
        token.is_self_verified = true;
        token.blockchain.add_block(genesis);
        dp.insert_token(vec![1], "token".to_string(), token);
    }

    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    // Deliberately NO `bootstrap_token`: the cache is cold, exactly like a
    // committer that just booted. The pending entry is materialized from the
    // wire block by the H4 sink, as in a split deployment.

    let tx_id = "tx_cold_boot";
    let mut tx = make_test_transaction(tx_id, b"alice".to_vec());
    tx.result_hash = vec![];
    let signed = SignedTransaction {
        shielded: None,
        transaction_id: tx_id.to_string(),
        transaction: tx,
        total_voters: 3,
        total_stake: 42,
        leader_hash: genesis_tip.clone(),
        leader_address: vec![],
        leader_stake: 0,
        finalizer_addr: vec![],
        finalizer_sig: TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![],
            current_stake: 0,
        },
        executor_sigs: HashMap::new(),
        proposer_key: vec![],
    };
    let mut block = Block {
        signed_trans: signed,
        token_metadata: HashMap::new(),
        previous_hash: genesis_tip.clone(),
        timestamp: 0,
        current_hash: vec![],
        finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block)
        .expect("well-formed block hashes");
    let committed_hash = block.current_hash.clone();

    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    assert!(
        committer.check_and_commit_transaction_results(&commit, vec![]).await.is_ok(),
        "a cold committer with the token in the data service must commit, not TokenNotFound"
    );
    let stored = dp.get_token(&vec![1], "token").expect("advanced token persisted");
    assert_eq!(stored.blockchain.chain.len(), 2);
    assert_eq!(stored.blockchain.get_current_chain_state().last_hash_in, committed_hash);
}

/// The cache-warm reads the data service; a token ABSENT there too stays the
/// fail-closed `TokenNotFound` — and the message must say the provider missed
/// too, so an operator does not chase a phantom cache bug.
#[tokio::test]
async fn cold_cache_commit_on_an_absent_token_fails_closed_naming_the_store() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    let tx_id = "tx_absent";
    let mut block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    block.signed_trans.leader_hash = vec![];
    block.previous_hash = vec![];
    block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block)
        .expect("rehash after field change");
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    match committer.check_and_commit_transaction_results(&commit, vec![]).await {
        Err(CommitterError::TokenNotFound(msg)) => {
            assert!(
                msg.contains("data service"),
                "the refusal must name the second miss, got: {msg}"
            );
        }
        other => panic!("expected TokenNotFound, got {other:?}"),
    }
}

/// A commit whose advanced token cannot be persisted is NOT a success: the
/// `TokenPersist` error surfaces instead of the block being reported
/// committed while the shared store stays stale.
#[tokio::test]
async fn commit_surfaces_token_persist_failure_instead_of_reporting_success() {
    let dp = Arc::new(
        TestDataProvider::with_failures(false, false).with_token_save_failure(true),
    );
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

    let tx_id = "tx_persist_fail";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    match committer.check_and_commit_transaction_results(&commit, vec![]).await {
        Err(CommitterError::TokenPersist { token_id, .. }) => {
            assert_eq!(token_id, "01", "the failing token must be identified (hex id 01)");
        }
        other => panic!("expected TokenPersist, got {other:?}"),
    }
}
