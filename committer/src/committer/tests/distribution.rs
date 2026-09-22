use super::helpers::*;
use super::super::*;
use std::sync::{Arc, Mutex};


/// (AUDIT Phase 5.5 / H13 discriminator) A `DistributeToken` carrying an id already present in
/// the local cache is refused and leaves the cached token — chain and metadata — untouched. The
/// old `self.tokens.insert` blindly overwrote the existing token, so any peer could swap in an
/// arbitrary chain/metadata under a settled id. Proven discriminator: restoring the blind insert
/// makes the handler return `Ok(())` and overwrites `name`/`asset_hash` (all value assertions
/// fail without the fix).
#[tokio::test]
async fn handle_token_distribution_rejects_conflicting_token_id() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, collector) = make_test_committer(dp);

    // Bootstrap the authoritative token: id vec![1], distinct metadata + asset_hash, and a seeded
    // genesis chain. Pin the tip and length so we can prove the cached chain survives.
    let mut original = Token::new();
    original.id = vec![1];
    original.set_metadata("name".to_string(), "original".to_string());
    original.asset_hash = vec![0x01u8; 32];
    committer.bootstrap_token(original);
    bootstrap_token_chain(&committer);
    let before_tip = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;
    let before_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();

    // A malicious token carries the SAME id but different metadata and asset_hash — a peer trying
    // to swap in an alternative token under the settled id.
    let mut malicious = Token::new();
    malicious.id = vec![1];
    malicious.set_metadata("name".to_string(), "swapped".to_string());
    malicious.asset_hash = vec![0xFFu8; 32];

    let body = serialize_to_bytes_rmp(&malicious).expect("Token serialization");
    let message = Message {
        chain_id: "test".to_string(),
        action: String::from("DistributeToken"),
        body,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let result = committer.handle_token_distribution(message).await;

    // The conflicting distribution is refused, and every part of the cached token is preserved.
    assert!(matches!(result, Err(CommitterError::TokenConflict(_))));
    let cached_ref = committer.tokens.get(&vec![1]).unwrap();
    let cached = cached_ref.value();
    assert_eq!(cached.metadata.get("name").map(String::as_str), Some("original"));
    assert_eq!(cached.asset_hash, vec![0x01u8; 32]);
    assert_eq!(
        cached.blockchain.get_current_chain_state().last_hash_in,
        before_tip
    );
    assert_eq!(cached.blockchain.get_count(), before_len);
    // The rejection is logged, not silent.
    assert!(collector
        .logs
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.contains("TOKEN REPLACEMENT REJECTED")));
}


/// (AUDIT Phase 5.5 / H13 positive guard) A `DistributeToken` whose id is not already cached is
/// accepted — this is how a node joining the network seeds a token it lacks. Guards against the
/// fix over-rejecting the legitimate seeding path.
#[tokio::test]
async fn handle_token_distribution_accepts_new_token_id() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // A pre-existing token under id vec![1].
    let mut existing = Token::new();
    existing.id = vec![1];
    existing.set_metadata("name".to_string(), "existing".to_string());
    committer.bootstrap_token(existing);

    // A distribution carrying a brand-new id vec![9].
    let mut newcomer = Token::new();
    newcomer.id = vec![9];
    newcomer.set_metadata("name".to_string(), "newcomer".to_string());

    let body = serialize_to_bytes_rmp(&newcomer).expect("Token serialization");
    let message = Message {
        chain_id: "test".to_string(),
        action: String::from("DistributeToken"),
        body,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let result = committer.handle_token_distribution(message).await;

    assert!(result.is_ok());
    let cached_ref = committer
        .tokens
        .get(&vec![9])
        .expect("newcomer token should be cached");
    let cached = cached_ref.value();
    assert_eq!(cached.metadata.get("name").map(String::as_str), Some("newcomer"));
}
