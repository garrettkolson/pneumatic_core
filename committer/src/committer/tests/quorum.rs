use super::helpers::*;
use super::super::*;
use std::sync::{Arc, Mutex};


#[tokio::test]
async fn handle_block_quorum_reached_updates_finality_status() {
    // Direct test of Blockchain::set_finality_status.
    // The handler's job is to find the block by hash and call this.
    let mut blockchain = pneumatic_core::blocks::Blockchain::new();

    // Build a block the same way the other tests do
    let block = make_test_block_for_token_internal(&blockchain);
    let block_hash = block.current_hash.clone();
    blockchain.add_block(block);

    // Verify initial status
    let tip = blockchain.get_block_at(0).unwrap();
    assert_eq!(tip.finality_status, FinalityStatus::Optimistic);

    // Transition to Confirmed
    let result = blockchain.set_finality_status(&block_hash, FinalityStatus::Confirmed);
    assert!(result.is_ok());

    // Verify final status
    let tip = blockchain.get_block_at(0).unwrap();
    assert_eq!(tip.finality_status, FinalityStatus::Confirmed);
}


#[tokio::test]
async fn handle_block_confirmed_vote_skips_missing_stake_set() {
    // When a vote arrives before BlockFinalized, it should be silently ignored
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // No BlockFinalized was received, so no stake set is cached
    let body = serialize_to_bytes_rmp(&(vec![1, 2, 3], vec![4, 5, 6]))
        .expect("Vote serialization");
    let message = Message {
        chain_id: "test".to_string(),
        action: String::from("BlockConfirmed"),
        body,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };

    // Should not error, just ignore
    let result = committer.handle_block_confirmed_vote(message).await;
    assert!(result.is_ok());
}


#[tokio::test]
async fn handle_block_confirmed_vote_accumulates_stake() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // Build block and cache stake set
    let block = make_gossip_block(&committer, "vote_test", b"vote".to_vec());
    let block_hash = block.current_hash.clone();

    // Cache a stake set: alice=100, bob=50, charlie=50 (total=200)
    let mut stake_set = StakeSet::default();
    stake_set.stakers.insert(b"alice".to_vec(), 100);
    stake_set.stakers.insert(b"bob".to_vec(), 50);
    stake_set.stakers.insert(b"charlie".to_vec(), 50);

    // Manually cache the stake set
    committer.stake_set_cache.lock().await
        .insert(block_hash.clone(), stake_set.clone());

    // First vote: alice (stake=100, should be below 67% quorum of 200 = 134)
    let body1 = serialize_to_bytes_rmp(&(block_hash.clone(), b"alice".to_vec()))
        .expect("Vote serialization");
    let msg1 = Message {
        chain_id: "test".to_string(),
        action: String::from("BlockConfirmed"),
        body: body1,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let result = committer.handle_block_confirmed_vote(msg1).await;
    assert!(result.is_ok());

    // Second vote: bob (cumulative=150, should cross quorum threshold)
    let body2 = serialize_to_bytes_rmp(&(block_hash.clone(), b"bob".to_vec()))
        .expect("Vote serialization");
    let msg2 = Message {
        chain_id: "test".to_string(),
        action: String::from("BlockConfirmed"),
        body: body2,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let result = committer.handle_block_confirmed_vote(msg2).await;
    assert!(result.is_ok());

    // Verify cumulative stake reached quorum (150 >= 134)
    let votes = committer.confirmation_votes.lock().await;
    let (keys, cumulative) = votes.get(&block_hash).expect("block_hash should have votes");
    assert_eq!(keys.len(), 2);
    assert_eq!(*cumulative, 150);
}


// AUDIT Phase 6.9 (Item B, positive-boundary discriminator): with total=200 @67%
// (threshold 134), a lone alice vote (100) must NOT reach quorum and a bob vote
// pushing cumulative to 150 MUST. Asserts the observable broadcast, so the exact
// integer comparison is exercised end-to-end.
#[tokio::test]
async fn handle_block_confirmed_vote_quorum_broadcast_boundary() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let block = make_gossip_block(&committer, "quorum_boundary", b"vote".to_vec());
    let block_hash = block.current_hash.clone();

    let mut stake_set = StakeSet::default();
    stake_set.stakers.insert(b"alice".to_vec(), 100);
    stake_set.stakers.insert(b"bob".to_vec(), 50);
    stake_set.stakers.insert(b"charlie".to_vec(), 50); // total = 200, 67% = 134
    committer.stake_set_cache.lock().await
        .insert(block_hash.clone(), stake_set.clone());

    // Sentinel records outbound messages so we can observe the quorum broadcast.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(committer.node_registry.register_peer(
        vec![0xCC; 32],
        [3u8; 16],
        &NodeRegistryType::Sentinel,
        Box::new(RecordingConnection { recorder: recorder.clone() }),
    ));

    fn quorum_broadcasts(recorder: &Arc<Mutex<Vec<Vec<u8>>>>) -> usize {
        recorder.lock().unwrap().iter().filter(|raw| {
            matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == "BlockQuorumReached")
        }).count()
    }

    // Vote as alice: cumulative 100 < 134 → no broadcast.
    let body_alice = serialize_to_bytes_rmp(&(block_hash.clone(), b"alice".to_vec())).expect("serialize");
    committer.handle_block_confirmed_vote(Message {
        chain_id: "test".to_string(),
        action: String::from("BlockConfirmed"),
        body: body_alice,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    }).await.expect("alice vote accepted");
    assert_eq!(quorum_broadcasts(&recorder), 0, "alice's 100 stake is below the 134 quorum");

    // Vote as bob: cumulative 150 >= 134 → broadcast fires.
    let body_bob = serialize_to_bytes_rmp(&(block_hash.clone(), b"bob".to_vec())).expect("serialize");
    committer.handle_block_confirmed_vote(Message {
        chain_id: "test".to_string(),
        action: String::from("BlockConfirmed"),
        body: body_bob,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    }).await.expect("bob vote accepted");
    assert_eq!(quorum_broadcasts(&recorder), 1, "bob pushes cumulative to 150, crossing quorum");
}


// AUDIT Phase 6.9 (Item B, precision discriminator — the real f64 bug).
// voter = 2^52, other = 2^52 + 1, so total = 2^53 + 1. quorum_percentage = 50.0.
// total is NOT representable in f64 (max exact integer is 2^53), so it rounds down
// to 2^53. A lone voter vote:
//   f64 (old): total as f64 rounds 2^53+1 -> 2^53; threshold = 2^53 * 50/100 = 2^52;
//              cumulative = 2^52;  2^52 >= 2^52 -> REACHED (broadcast wrongly fires).
//   integer (new): voter*100 = 450359962737049600 <
//                  total*50 = 9007199254740993*50 = 450359962737049650 -> NOT reached.
// Assert NO broadcast. Temp-reverting to f64 makes the broadcast fire -> assert fails.
#[tokio::test]
async fn handle_block_confirmed_vote_quorum_precision_big_stakes() {
    let dp = Arc::new(TestDataProvider::new());
    let (mut committer, _registry, _logger) = make_test_committer(dp);

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // Override to 50% so the 2^52/2^52 boundary is exactly reachable under f64.
    let mut env = (*committer.env_data).clone();
    env.quorum_percentage = 50.0;
    committer.env_data = Arc::new(env);

    let block = make_gossip_block(&committer, "quorum_precision", b"vote".to_vec());
    let block_hash = block.current_hash.clone();

    // two adjacent values whose sum 2^53+1 is not representable in f64 (rounds to 2^53).
    let voter: u64 = 4503599627370496; // 2^52
    let other: u64 = 4503599627370497; // 2^52 + 1
    let mut stake_set = StakeSet::default();
    stake_set.stakers.insert(b"voter".to_vec(), voter);
    stake_set.stakers.insert(b"other".to_vec(), other);
    committer.stake_set_cache.lock().await
        .insert(block_hash.clone(), stake_set.clone());

    let recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(committer.node_registry.register_peer(
        vec![0xCC; 32],
        [3u8; 16],
        &NodeRegistryType::Sentinel,
        Box::new(RecordingConnection { recorder: recorder.clone() }),
    ));

    // A lone voter vote: integer math says NOT reached (no broadcast).
    let body_voter = serialize_to_bytes_rmp(&(block_hash.clone(), b"voter".to_vec())).expect("serialize");
    committer.handle_block_confirmed_vote(Message {
        chain_id: "test".to_string(),
        action: String::from("BlockConfirmed"),
        body: body_voter,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    }).await.expect("voter vote accepted");

    let reached = recorder.lock().unwrap().iter().any(|raw| {
        matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == "BlockQuorumReached")
    });
    assert!(!reached, "integer quorum must NOT be reached for a 2^53 vote on a 2^53+1 total at 50%");
}

// --- Phase 0 item 3: the denominator is declared, and a claim proves nothing ---

/// Cast a `BlockConfirmed` vote for `voter` on `block_hash`.
async fn cast_vote(committer: &Committer, block_hash: &[u8], voter: &[u8]) {
    let body = serialize_to_bytes_rmp(&(block_hash.to_vec(), voter.to_vec())).expect("serialize");
    committer
        .handle_block_confirmed_vote(Message {
            chain_id: "test".to_string(),
            action: String::from("BlockConfirmed"),
            body,
            signature: vec![],
            public_key: voter.to_vec(),
            stake_set: None
        })
        .await
        .expect("vote should be handled");
}

fn count_quorum_broadcasts(recorder: &Arc<Mutex<Vec<Vec<u8>>>>) -> usize {
    recorder
        .lock()
        .unwrap()
        .iter()
        .filter(|raw| {
            matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == "BlockQuorumReached")
        })
        .count()
}

fn tip_finality(committer: &Committer) -> pneumatic_core::blocks::FinalityStatus {
    let entry = committer.tokens.get(&vec![1]).expect("token 1");
    let count = entry.value().blockchain.get_count();
    entry
        .value()
        .blockchain
        .get_block_at(count - 1)
        .expect("tip block")
        .finality_status
        .clone()
}

fn tip_hash(committer: &Committer) -> Vec<u8> {
    let entry = committer.tokens.get(&vec![1]).expect("token 1");
    let count = entry.value().blockchain.get_count();
    entry
        .value()
        .blockchain
        .get_block_at(count - 1)
        .expect("tip block")
        .current_hash
        .clone()
}

/// The hole this closes: `handle_block_quorum_reached` used to read "No further
/// quorum check needed — the broadcaster verified quorum", and the role gate let
/// ANY registered node be that broadcaster. One message could therefore mark a
/// block final for every committer without a single vote. Now the claim is a hint
/// and the receiver re-runs its own arithmetic.
#[tokio::test]
async fn quorum_claim_is_refused_unless_this_node_computes_quorum_too() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let block_hash = tip_hash(&committer);

    // total = 300, quorum 67% → 201 required.
    let mut stake_set = StakeSet::default();
    stake_set.stakers.insert(b"alice".to_vec(), 100);
    stake_set.stakers.insert(b"bob".to_vec(), 100);
    stake_set.stakers.insert(b"charlie".to_vec(), 100);
    committer
        .stake_set_cache
        .lock()
        .await
        .insert(block_hash.clone(), stake_set);

    let recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(committer.node_registry.register_peer(
        vec![0xCC; 32],
        [3u8; 16],
        &NodeRegistryType::Sentinel,
        Box::new(RecordingConnection { recorder: recorder.clone() }),
    ));

    let claim = |hash: &Vec<u8>| Message {
        chain_id: "test".to_string(),
        action: String::from("BlockQuorumReached"),
        body: serialize_to_bytes_rmp(hash).expect("serialize"),
        signature: vec![],
        // Someone who holds no stake at all, claiming quorum.
        public_key: b"passerby".to_vec(),
        stake_set: None
    };

    // One vote is 100 of 300 — a long way short of 201.
    cast_vote(&committer, &block_hash, b"alice").await;

    committer
        .handle_block_quorum_reached(claim(&block_hash))
        .await
        .expect("the claim is handled, just not obeyed");
    assert_eq!(
        committer.rejected_quorum_claim_count(),
        1,
        "a claim that contradicts local arithmetic must be counted"
    );
    assert_ne!(
        tip_finality(&committer),
        pneumatic_core::blocks::FinalityStatus::Confirmed,
        "a claim alone must not upgrade finality"
    );
    assert_eq!(
        count_quorum_broadcasts(&recorder),
        0,
        "refusing a claim must not re-broadcast it"
    );

    // Now the votes really are in: 300 >= 201. The node reaches its own
    // conclusion and announces it, and only then does a claim take effect.
    cast_vote(&committer, &block_hash, b"bob").await;
    cast_vote(&committer, &block_hash, b"charlie").await;
    assert_eq!(
        committer.local_quorum_reached(&block_hash).await,
        Some(true),
        "300 of 300 must satisfy 67%"
    );
    assert!(
        count_quorum_broadcasts(&recorder) >= 1,
        "this node must broadcast quorum it computed itself"
    );

    committer
        .handle_block_quorum_reached(claim(&block_hash))
        .await
        .expect("handled");
    assert_eq!(
        tip_finality(&committer),
        pneumatic_core::blocks::FinalityStatus::Confirmed,
        "a claim backed by this node's own quorum must upgrade the block"
    );
    assert_eq!(
        committer.rejected_quorum_claim_count(),
        1,
        "only the unverifiable claim counts; the verified one must not"
    );
}

/// A committer's own confirmation used to be skipped by the vote handler with the
/// comment "we already voted via handle_block_finalized" — but that path only
/// broadcast, it never recorded. So self stake sat in the denominator and self
/// votes could never reach the numerator. With equal stake, three committers could
/// each see only 2 of 3 (66.7% < 67%) and NOTHING would ever become `Confirmed`.
///
/// The votes here are cast BEFORE the block arrives, so this also pins that
/// `handle_block_finalized` replays them: without either half of the fix the
/// cumulative is 200 and no broadcast happens.
#[tokio::test]
async fn block_finalized_counts_our_own_vote_and_replays_early_ones() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let self_key = committer.public_key.clone();

    // Three equal stakers: this node, plus two peers. total = 300, 67% → 201.
    let mut stake_set = StakeSet::default();
    stake_set.stakers.insert(self_key.clone(), 100);
    stake_set.stakers.insert(b"alice".to_vec(), 100);
    stake_set.stakers.insert(b"bob".to_vec(), 100);

    let recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(committer.node_registry.register_peer(
        vec![0xCC; 32],
        [3u8; 16],
        &NodeRegistryType::Sentinel,
        Box::new(RecordingConnection { recorder: recorder.clone() }),
    ));

    let block = make_gossip_block(&committer, "own_vote_tx", b"finalizer".to_vec());
    let block_hash = block.current_hash.clone();

    // Both peers vote before the block lands. There is no stake set yet, so these
    // used to be dropped on the floor with `return Ok(())`.
    cast_vote(&committer, &block_hash, b"alice").await;
    cast_vote(&committer, &block_hash, b"bob").await;
    assert_eq!(
        committer.buffered_confirmation_vote_count(),
        2,
        "both early votes must be buffered, not discarded"
    );
    assert!(
        committer.confirmation_votes.lock().await.is_empty(),
        "nothing can be scored before the stake set exists"
    );

    // The block arrives with the stake set: cache it, replay the votes, and cast
    // this node's own — which is what tips 200 over the 201 threshold.
    let body = serialize_to_bytes_rmp(&block).expect("Block serialization");
    let message = Message {
        chain_id: "test".to_string(),
        action: String::from("BlockFinalized"),
        body,
        signature: vec![],
        public_key: vec![],
        stake_set: Some(stake_set)
    };
    committer
        .handle_block_finalized(message)
        .await
        .expect("a valid block should commit");

    {
        let votes = committer.confirmation_votes.lock().await;
        let (keys, cumulative) = votes
            .get(&block_hash)
            .expect("the block must have accumulated votes after replay");
        assert_eq!(keys.len(), 3, "both replayed peers AND this node's own vote");
        assert_eq!(*cumulative, 300, "self stake must count: 200 would mean self was skipped");
    }
    assert!(
        count_quorum_broadcasts(&recorder) >= 1,
        "quorum (300 >= 201) must be announced by this node"
    );
}

/// Early-vote buffering on its own, so the end-to-end test above cannot pass by
/// accident if the replay path changes shape.
#[tokio::test]
async fn a_vote_before_its_stake_set_is_buffered_then_replayed() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let block_hash = tip_hash(&committer);

    cast_vote(&committer, &block_hash, b"alice").await;
    assert_eq!(committer.buffered_confirmation_vote_count(), 1);
    assert!(committer
        .pending_confirmation_votes
        .lock()
        .await
        .contains_key(&block_hash));

    let mut stake_set = StakeSet::default();
    stake_set.stakers.insert(b"alice".to_vec(), 100);
    stake_set.stakers.insert(b"bob".to_vec(), 100);
    committer
        .stake_set_cache
        .lock()
        .await
        .insert(block_hash.clone(), stake_set);

    committer.replay_pending_votes(&block_hash).await;

    assert!(
        committer
            .pending_confirmation_votes
            .lock()
            .await
            .is_empty(),
        "a replayed block's buffer must be drained"
    );
    {
        let votes = committer.confirmation_votes.lock().await;
        let (keys, cumulative) = votes.get(&block_hash).expect("vote must be replayed");
        assert_eq!(keys.len(), 1);
        assert_eq!(*cumulative, 100);
    }
    // The stake set alone is not a quorum, and replay must not invent one.
    assert_eq!(committer.local_quorum_reached(&block_hash).await, Some(false));
}

/// Pins the sender policy itself: `BlockConfirmed` is a vote and every role sends
/// one; `BlockQuorumReached` is a conclusion and only the role that computes it
/// may send it. Enforcement of `Exact` is shared with the other gated actions; this
/// pins the mapping, which is the part that was wrong.
#[test]
fn only_a_committer_may_claim_quorum_reached() {
    assert!(
        matches!(
            allowed_senders_for("BlockQuorumReached"),
            AllowedSenders::Exact(NodeRegistryType::Committer)
        ),
        "a quorum claim must be restricted to the role that verifies quorum"
    );
    assert!(
        matches!(allowed_senders_for("BlockConfirmed"), AllowedSenders::AnyRegistered),
        "BlockConfirmed is a vote from every validating node — tightening it \
         would silence the finalizer's own vote"
    );
}
