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
