use super::helpers::*;
use super::super::*;
use std::sync::{Arc, Mutex};


#[tokio::test]
async fn commit_no_conflict_inserts_candidate() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_no_conflict";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    // Insert the existing candidate into the registry (simulating pre-existing)
    let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
        let token = entry.value();
        token.blockchain.get_current_chain_state().last_hash_in
    } else {
        vec![42u8; 32]
    };
    // Distinct stakes make the outcome deterministic: the incoming block
    // (proposer vec![], stake 500) wins over the existing candidate (proposer vec![10], stake 100).
    committer.stake_store.add_staker(vec![10], 100);
    committer.stake_store.add_staker(vec![], 500);

    let existing_block = make_block_with_proposer(&committer, "tx_existing", vec![10]);
    committer.candidate_registry.insert(
        vec![1], prev_hash.clone(), existing_block, vec![10],
    );

    // Commit — the conflict is detected and resolved. The winner commits and the
    // loser group is cleared (AUDIT Phase 5.2 / H2). The verified proposer (finalizer_key)
    // is the incoming block's self-declared proposer, vec![], matching its 500-stake identity.
    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(result.is_ok(), "higher-stake incoming block wins the conflict");
    assert_eq!(
        committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
        0,
        "the resolved loser candidate group must be cleared"
    );
    assert_eq!(
        committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
        2,
        "only the winning block commits; the loser is never appended"
    );
}


#[tokio::test]
async fn commit_first_block_on_fresh_token_without_bootstrap() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp);

    // Bootstrap the token only — the chain stays empty
    let mut token = Token::new();
    token.id = vec![1];
    // Fail-closed validation (AUDIT Phase 3.2 / C5): the genesis commit only
    // validates under the "SelfSigned" spec when the token is flagged
    // is_self_verified.
    token.is_self_verified = true;
    committer.bootstrap_token(token);

    let tx_id = "tx_first_block";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

    // On an empty chain the helper emits previous_hash = vec![]
    // (genesis convention)
    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    assert!(block.previous_hash.is_empty());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    // Block 1 must commit through the standard path
    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(result.is_ok());

    let entry = committer.tokens.get(&vec![1]).unwrap();
    assert_eq!(entry.value().blockchain.get_count(), 1);
}


#[tokio::test]
async fn commit_conflict_different_stakes_discards_loser() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_conflict_stake";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

    let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
        let token = entry.value();
        token.blockchain.get_current_chain_state().last_hash_in
    } else {
        vec![42u8; 32]
    };

    // Add proposers with different stakes to StakeStore
    committer.stake_store.add_staker(vec![10], 100);  // existing proposer (low stake)
    committer.stake_store.add_staker(b"alice".to_vec(), 500);  // new proposer (high stake)

    // Insert existing candidate with lower stake
    let existing_block = make_block_with_proposer(&committer, "tx_existing", vec![10]);
    committer.candidate_registry.insert(
        vec![1], prev_hash.clone(), existing_block, vec![10],
    );

    let block = make_block_with_proposer(&committer, tx_id, b"alice".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, b"alice".to_vec()).await;
    assert!(result.is_ok(), "higher-stake incoming block wins the conflict");

    // AUDIT Phase 5.2 / H2: the loser is discarded — only the winning block commits
    // (chain grew by exactly one) and the resolved candidate group is cleared, so
    // exactly one block remains at this position (no fork survives).
    assert_eq!(
        committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
        2,
        "only the winning block commits; the losing proposal is never appended"
    );
    assert_eq!(
        committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
        0,
        "the resolved loser candidate group must be cleared"
    );
}


#[tokio::test]
async fn commit_conflict_same_proposer_emits_slash() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_double_sign";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

    let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
        let token = entry.value();
        token.blockchain.get_current_chain_state().last_hash_in
    } else {
        vec![42u8; 32]
    };

    // Same proposer — double-signed scenario
    committer.stake_store.add_staker(vec![10], 100);

    // Insert existing candidate with same proposer key
    let existing_block = make_block_with_proposer(&committer, "tx_existing", vec![10]);
    committer.candidate_registry.insert(
        vec![1], prev_hash.clone(), existing_block, vec![10],
    );

    let block = make_block_with_proposer(&committer, tx_id, vec![10]); // SAME proposer!
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![10]).await;
    // AUDIT Phase 5.2 / H2: a same-proposer double-signed re-proposal is rejected
    // on the commit path (the winner stays as the tip, the loser is discarded).
    assert!(
        matches!(result, Err(CommitterError::LoserDiscarded)),
        "a double-signed re-proposal must be discarded on the commit path"
    );

    // AUDIT Phase 5.1 / H1: the slash is still applied even though the block is
    // rejected — the double-signed proposer's stake must actually drop to zero.
    assert_eq!(
        committer.stake_store.get_stake(&vec![10]),
        0,
        "full-stake slash should zero the offender's stake"
    );
    // And the resolved candidate group is cleared.
    assert_eq!(
        committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
        0,
        "the double-signed candidate group must be cleared"
    );
}


#[tokio::test]
async fn commit_conflict_same_proposer_partial_slash_respects_fraction() {
    // AUDIT Phase 5.1 / H1: the slash amount is the configured fraction of
    // the offender's stake, not a hardcoded value. With slash_fraction = 0.5
    // a proposer staked at 100 should drop to 50, proving the amount is
    // configured rather than always full.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer_with_slash(dp, 0.5);

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_double_sign_partial";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

    let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
        let token = entry.value();
        token.blockchain.get_current_chain_state().last_hash_in
    } else {
        vec![42u8; 32]
    };

    // Same proposer — double-signed scenario
    committer.stake_store.add_staker(vec![10], 100);

    // Insert existing candidate with same proposer key
    let existing_block = make_block_with_proposer(&committer, "tx_existing", vec![10]);
    committer.candidate_registry.insert(
        vec![1], prev_hash.clone(), existing_block, vec![10],
    );

    let block = make_block_with_proposer(&committer, tx_id, vec![10]); // SAME proposer!
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![10]).await;
    // AUDIT Phase 5.2 / H2: the double-signed re-proposal is rejected on the commit path.
    assert!(
        matches!(result, Err(CommitterError::LoserDiscarded)),
        "a double-signed re-proposal must be discarded on the commit path"
    );

    // The partial slash (0.5) is still applied even though the block is rejected.
    assert_eq!(
        committer.stake_store.get_stake(&vec![10]),
        50,
        "0.5 fraction of 100 should leave 50"
    );
    assert_eq!(
        committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
        0,
        "the double-signed candidate group must be cleared"
    );
}


#[tokio::test]
async fn commit_no_existing_candidates_inserts_first() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_first";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

    let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let prev_hash = block.previous_hash.clone();
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
    assert!(result.is_ok());

    // First candidate should be inserted into the registry
    assert_eq!(
        committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
        1,
    );
}


// --- Conflict resolution: discard losers + bound the registry (AUDIT Phase 5.2 / H2) ---

#[tokio::test]
async fn commit_conflict_rolls_back_loser_tip_and_commits_winner() {
    // AUDIT Phase 5.2 / H2, design decision #1: when the losing proposal is the
    // current chain tip, committing the winner rolls that tip back (guarded on a hash
    // match) and appends the winner, so exactly one block remains at the position and
    // the tip advances to the winner's hash — the loser is never left appended.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp);

    // Bootstrap token + genesis chain.
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // The genesis tip is the (token_id, previous_hash) position both proposals fight over.
    let prev_tip = committer.tokens.get(&vec![1]).unwrap().value()
        .blockchain.get_current_chain_state().last_hash_in;

    // Two competing proposals at the same position (genesis tip), distinct proposers.
    // Build both BEFORE committing so each chains off the genesis tip (they are siblings).
    let tx_a = "tx_roll_a";
    let block_a = make_block_with_proposer(&committer, tx_a, b"alpha".to_vec());
    let tx_b = "tx_roll_b";
    let block_b = make_block_with_proposer(&committer, tx_b, b"beta".to_vec());
    assert_eq!(block_a.previous_hash, prev_tip, "block_a is a sibling of block_b");
    assert_eq!(block_b.previous_hash, prev_tip, "block_b is a sibling of block_a");

    // Give block_a the finalizing entry and commit it: it becomes the chain tip AND is
    // recorded as the first candidate at prev_tip.
    make_finalizing_entry(&registry, tx_a, b"alice".to_vec());
    committer.stake_store.add_staker(b"alpha".to_vec(), 100);
    let commit_a = TransactionCommit {
        trans_id: tx_a.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block_a,
    };
    assert!(committer.check_and_commit_transaction_results(&commit_a, b"alpha".to_vec()).await.is_ok());
    let tip_after_a = committer.tokens.get(&vec![1]).unwrap().value()
        .blockchain.get_current_chain_state().last_hash_in;
    assert_eq!(tip_after_a, commit_a.proposed_block.current_hash, "block_a is now the tip");

    // Now block_b (higher stake) is proposed at the same position.
    make_finalizing_entry(&registry, tx_b, b"alice".to_vec());
    committer.stake_store.add_staker(b"beta".to_vec(), 500);
    let commit_b = TransactionCommit {
        trans_id: tx_b.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block_b,
    };
    let result = committer.check_and_commit_transaction_results(&commit_b, b"beta".to_vec()).await;
    assert!(result.is_ok(), "higher-stake block_b wins the conflict and commits");

    // The loser (block_a) that was the tip is rolled back; block_b is the sole block at
    // the position; the candidate group is cleared.
    let tip = committer.tokens.get(&vec![1]).unwrap().value()
        .blockchain.get_current_chain_state().last_hash_in;
    assert_eq!(tip, commit_b.proposed_block.current_hash,
        "the winner's hash is the new tip (the rolled-back loser is gone)");
    assert_eq!(
        committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
        2,
        "genesis + winner: the rolled-back loser leaves exactly one appended block"
    );
    assert_eq!(
        committer.candidate_registry.candidate_count(&vec![1], &prev_tip),
        0,
        "the resolved candidate group must be cleared"
    );
}


#[tokio::test]
async fn commit_conflict_rejects_losing_commit() {
    // AUDIT Phase 5.2 / H2: when the incoming block LOSES its conflict it is rejected
    // with `LoserDiscarded`, the existing (winning) block stays as the tip, and the
    // resolved candidate group is cleared — the losing commit never reaches the chain.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp);

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // genesis tip is the contested position.
    let prev_tip = committer.tokens.get(&vec![1]).unwrap().value()
        .blockchain.get_current_chain_state().last_hash_in;

    let tx_a = "tx_rej_a";
    let tx_b = "tx_rej_b";
    let block_a = make_block_with_proposer(&committer, tx_a, b"alpha".to_vec());
    let block_b = make_block_with_proposer(&committer, tx_b, b"beta".to_vec());
    assert_eq!(block_a.previous_hash, prev_tip);
    assert_eq!(block_b.previous_hash, prev_tip);

    // block_a: HIGH stake (it will win); block_b: LOW stake (it will lose).
    make_finalizing_entry(&registry, tx_a, b"alice".to_vec());
    committer.stake_store.add_staker(b"alpha".to_vec(), 500);
    let commit_a = TransactionCommit {
        trans_id: tx_a.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block_a,
    };
    assert!(committer.check_and_commit_transaction_results(&commit_a, b"alpha".to_vec()).await.is_ok());
    let tip_after_a = committer.tokens.get(&vec![1]).unwrap().value()
        .blockchain.get_current_chain_state().last_hash_in;
    assert_eq!(tip_after_a, commit_a.proposed_block.current_hash);

    // block_b (lower stake) is the loser — it must be rejected, not appended.
    make_finalizing_entry(&registry, tx_b, b"alice".to_vec());
    committer.stake_store.add_staker(b"beta".to_vec(), 100);
    let commit_b = TransactionCommit {
        trans_id: tx_b.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block_b,
    };
    let result = committer.check_and_commit_transaction_results(&commit_b, b"beta".to_vec()).await;
    assert!(
        matches!(result, Err(CommitterError::LoserDiscarded)),
        "a lower-stake losing commit must be discarded on the commit path"
    );

    // The tip is unchanged (block_a stays); the loser was never appended; the group cleared.
    let tip = committer.tokens.get(&vec![1]).unwrap().value()
        .blockchain.get_current_chain_state().last_hash_in;
    assert_eq!(tip, commit_a.proposed_block.current_hash, "block_a remains the tip");
    assert_eq!(
        committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
        2,
        "only block_a is appended; the losing commit never reaches the chain"
    );
    assert_eq!(
        committer.candidate_registry.candidate_count(&vec![1], &prev_tip),
        0,
        "the resolved candidate group must be cleared"
    );
}


#[tokio::test]
async fn commit_conflict_uses_verified_proposer_not_forged_key() {
    // AUDIT Phase 5.8 / M10 discriminator: the incoming block's self-declared
    // `proposer_key` claims a high-stake identity (beta, 500) that the signed envelope
    // does NOT actually hold — the authenticated sender is delta (unregistered, stake 0).
    // Resolution must key off the *verified* sender (delta), never the unsigned self-
    // declared key, so a forged high-stake proposer_key can no longer steer the branch.
    //
    // On code that trusts self-declared proposer_key: beta (500) beats alpha (100) → the
    // forged block commits (is_ok): the attacker steers the branch. On fixed code: the
    // verified sender delta (0) loses to alpha (100) → the forged block is rejected.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp);

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_forged_key";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

    let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
        let token = entry.value();
        token.blockchain.get_current_chain_state().last_hash_in
    } else {
        vec![42u8; 32]
    };

    // alpha: the real, winning candidate (stake 100). beta: a forged self-declared
    // identity the incoming claims (stake 500) that no verified sender holds.
    committer.stake_store.add_staker(b"alpha".to_vec(), 100);
    committer.stake_store.add_staker(b"beta".to_vec(), 500);

    let existing_block = make_block_with_proposer(&committer, "tx_existing", b"alpha".to_vec());
    committer.candidate_registry.insert(
        vec![1], prev_hash.clone(), existing_block, b"alpha".to_vec(),
    );

    // Incoming self-declared proposer = beta (forged, 500); verified sender (finalizer_key)
    // = delta — not registered, so stake 0.
    let block = make_block_with_proposer(&committer, tx_id, b"beta".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    // Verified proposer (delta, 0) loses to alpha (100) → incoming rejected.
    let result = committer.check_and_commit_transaction_results(&commit, b"delta".to_vec()).await;
    assert!(
        matches!(result, Err(CommitterError::LoserDiscarded)),
        "a forged high-stake proposer_key (beta) must not steer resolution; the incoming (verified delta) is rejected"
    );
}


#[tokio::test]
async fn commit_conflict_folds_over_all_candidates() {
    // AUDIT Phase 5.8 / M10 discriminator: two competing candidates sit at one position.
    // candidates[0] = alpha (stake 100), candidates[1] = beta (stake 500); the incoming
    // (verified delta) has stake 300 — it beats alpha but loses to beta.
    //
    // On code that compares only candidates[0]: delta (300) beats alpha (100) → commits.
    // On fixed code (fold over ALL candidates): delta loses to beta (500) → rejected.
    // Order is deterministic here because the CandidateRegistry stores candidates in
    // insertion order (push), so alpha is candidates[0] and beta is candidates[1].
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, _logger) = make_test_committer(dp);

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "tx_fold";
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

    let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
        let token = entry.value();
        token.blockchain.get_current_chain_state().last_hash_in
    } else {
        vec![42u8; 32]
    };

    committer.stake_store.add_staker(b"alpha".to_vec(), 100);
    committer.stake_store.add_staker(b"beta".to_vec(), 500);
    committer.stake_store.add_staker(b"delta".to_vec(), 300);

    // Insert alpha (weak) first, then beta (strong): candidates[0]=alpha, candidates[1]=beta.
    let c0 = make_block_with_proposer(&committer, "tx_c0", b"alpha".to_vec());
    let c1 = make_block_with_proposer(&committer, "tx_c1", b"beta".to_vec());
    committer.candidate_registry.insert(vec![1], prev_hash.clone(), c0, b"alpha".to_vec());
    committer.candidate_registry.insert(vec![1], prev_hash.clone(), c1, b"beta".to_vec());

    // Incoming verified sender = delta (300).
    let block = make_block_with_proposer(&committer, tx_id, b"delta".to_vec());
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    // Fold: delta beats alpha but loses to beta → rejected (LoserDiscarded).
    let result = committer.check_and_commit_transaction_results(&commit, b"delta".to_vec()).await;
    assert!(
        matches!(result, Err(CommitterError::LoserDiscarded)),
        "incoming must be rejected: it loses to candidates[1] (beta) even though it beats candidates[0] (alpha)"
    );
}


#[tokio::test]
async fn candidate_registry_bounded_under_repeated_conflicts() {
    // AUDIT Phase 5.2 / H2: repeatedly populating one (token_id, previous_hash) position
    // with competing proposals (the shape a sustained conflict/storm produces) must not let
    // the candidate group grow without bound. The CandidateRegistry caps each position at
    // DEFAULT_MAX_CANDIDATES via LRU eviction (oldest evicted). The commit path inserts
    // (no-conflict branch) and reads (conflict branch) through this same primitive, so a
    // bounded registry bounds the conflict surface at commit time. This drives `insert`
    // directly to exercise the cap; see `commit_conflict_rolls_back_loser_tip_and_commits_winner`
    // and `commit_conflict_rejects_losing_commit` for the resolved-group-cleared behavior on
    // the actual commit path.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _, _) = make_test_committer(dp);

    // Bootstrap a token so the position is realistic.
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);
    let prev_hash = committer.tokens.get(&vec![1]).unwrap().value()
        .blockchain.get_current_chain_state().last_hash_in;

    // Insert far more competing proposals at this one position than the cap allows.
    const N: usize = CandidateRegistry::DEFAULT_MAX_CANDIDATES + 16;
    for i in 0..N {
        let block = make_block_with_proposer(&committer, &format!("tx_bnd_{i}"), vec![i as u8]);
        committer.candidate_registry.insert(vec![1], prev_hash.clone(), block, vec![i as u8]);
    }

    // The position is capped at DEFAULT_MAX_CANDIDATES, and the oldest 16 were evicted (LRU):
    // each of tx_bnd_0..tx_bnd_15 is absent from the survivors, while the newest ones remain.
    for i in 0..16 {
        let present = committer.candidate_registry.get_candidates(&vec![1], &prev_hash)
            .iter()
            .any(|(block, _)| block.signed_trans.transaction_id == format!("tx_bnd_{i}"));
        assert!(!present, "oldest proposal tx_bnd_{i} must be evicted under LRU eviction");
    }
}
