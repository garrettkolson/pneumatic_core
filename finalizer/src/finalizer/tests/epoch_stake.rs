//! Epoch/stake-cache and core-finalization tests: shutdown, stake-set
//! cache fetch/invalidation, per-epoch stake-set fallback, previous-hash and
//! stake-metrics resolution, and the optimistic finalization path.
use super::helpers::*;
use super::super::*;

#[tokio::test]
async fn test_shutdown() {
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer(pending_registry);

    assert!(!finalizer.is_shutting_down().await);

    finalizer.initiate_shutdown().await;
    assert!(finalizer.is_shutting_down().await);
}

/// Build a block with a real `current_hash` (computed via
/// `BlockFactory`) so a token's chain validates as a normal (non-empty,
/// valid) chain.
fn make_hashed_block(previous_hash: Vec<u8>) -> Block {
    let mut block = Block {
        signed_trans: SignedTransaction::test_transaction(),
        token_metadata: HashMap::new(),
        previous_hash,
        current_hash: vec![],
        timestamp: 1000,
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash = BlockFactory::create_hash(&block)
        .expect("well-formed test block hash");
    block
}

#[tokio::test]
async fn test_stake_cache_fetched_from_data_provider() {
    let pending_registry = make_test_pending_registry();
    let data_provider = Arc::new(
        StubDataProvider::new().with_stake_snapshot(0, make_stake_set(vec![(b"executor_1".to_vec(), 100)])),
    );
    let finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

    // First call — DataProvider fallback, result cached locally
    let fetched = finalizer.get_stake_set_for_epoch().unwrap();
    assert_eq!(fetched.total_stake(), 100);
    assert_eq!(finalizer.stake_cache.cached_count(), 1);

    // Second call — local cache hit, same stake set
    let fetched = finalizer.get_stake_set_for_epoch().unwrap();
    assert_eq!(fetched.total_stake(), 100);
}

#[tokio::test]
async fn test_stake_cache_invalidated_on_advance_epoch() {
    let pending_registry = make_test_pending_registry();
    let data_provider = Arc::new(
        StubDataProvider::new()
            .with_stake_snapshot(0, make_stake_set(vec![(vec![1], 100)]))
            .with_stake_snapshot(1, make_stake_set(vec![(vec![2], 200)])),
    );
    let mut finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

    // Prime the cache with epoch 0's snapshot
    assert_eq!(finalizer.get_stake_set_for_epoch().unwrap().total_stake(), 100);
    assert_eq!(finalizer.stake_cache.cached_count(), 1);

    finalizer.advance_epoch();
    assert_eq!(finalizer.stake_cache.cached_count(), 0);

    // Next fetch pulls the new epoch's snapshot fresh from the DataProvider
    assert_eq!(finalizer.get_stake_set_for_epoch().unwrap().total_stake(), 200);
    assert_eq!(finalizer.stake_cache.cached_count(), 1);
}

#[test]
fn test_get_stake_set_falls_back_to_manual_override() {
    let pending_registry = make_test_pending_registry();
    let data_provider = Arc::new(
        StubDataProvider::new().with_stake_snapshot(0, make_stake_set(vec![(vec![1], 100)])),
    );
    let mut finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

    // Manual override takes priority over the cache (backward compat for tests)
    finalizer.set_stake_set(make_stake_set(vec![(vec![9], 50)]));
    assert_eq!(finalizer.get_stake_set_for_epoch().unwrap().total_stake(), 50);
}

#[test]
fn test_resolve_previous_hash_returns_chain_tip() {
    let pending_registry = make_test_pending_registry();
    let expected;
    let token = {
        let mut token = Token::new().with_id(vec![0, 1, 2]);
        token.blockchain.add_block(make_hashed_block(vec![]));
        expected = token.blockchain.get_current_chain_state().last_hash_in;
        token
    };
    assert!(!expected.is_empty());
    let data_provider = Arc::new(
        StubDataProvider::new().with_token(vec![0, 1, 2], "test_env".to_string(), token),
    );
    let finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

    assert_eq!(finalizer.resolve_previous_hash(&vec![0, 1, 2]), expected);
}

#[test]
fn test_resolve_previous_hash_empty_chain() {
    let pending_registry = make_test_pending_registry();
    let data_provider = Arc::new(
        StubDataProvider::new()
            .with_token(vec![0, 1, 2], "test_env".to_string(), Token::new().with_id(vec![0, 1, 2])),
    );
    let finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

    // Empty chain → last_hash_in is vec![] (natural genesis value)
    assert_eq!(finalizer.resolve_previous_hash(&vec![0, 1, 2]), Vec::<u8>::new());
}

#[test]
fn test_resolve_previous_hash_missing_token_falls_back() {
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer(pending_registry); // empty DataProvider

    // Token not in the provider → graceful fallback to empty prev-hash
    assert_eq!(finalizer.resolve_previous_hash(&vec![0, 1, 2]), Vec::<u8>::new());
}

#[test]
fn test_resolve_previous_hash_invalid_chain_falls_back() {
    let pending_registry = make_test_pending_registry();
    let token = {
        let mut token = Token::new().with_id(vec![0, 1, 2]);
        let mut block = make_hashed_block(vec![]);
        // Deliberately corrupt the hash so the chain validates as invalid
        block.current_hash = vec![9, 9, 9];
        token.blockchain.add_block(block);
        token
    };
    let data_provider = Arc::new(
        StubDataProvider::new().with_token(vec![0, 1, 2], "test_env".to_string(), token),
    );
    let finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

    // ChainState::invalid() → last_hash_in is vec![]
    assert_eq!(finalizer.resolve_previous_hash(&vec![0, 1, 2]), Vec::<u8>::new());
}

#[test]
fn test_resolve_stake_metrics() {
    let pending_registry = make_test_pending_registry();
    let mut finalizer = make_finalizer(pending_registry);

    // No stake data → (0, 0)
    assert_eq!(finalizer.resolve_stake_metrics(), (0, 0));

    // Stake set with two stakers → (300, 2)
    finalizer.set_stake_set(make_stake_set(vec![(vec![1], 100), (vec![2], 200)]));
    assert_eq!(finalizer.resolve_stake_metrics(), (300, 2));
}

#[tokio::test]
async fn test_optimistic_finalize_with_seeded_token() {
    let node_registry = make_test_node_registry();
    let voter = NodeIdentity::generate_in_memory();
    register_executor(&node_registry, &voter);
    let data_provider = Arc::new(
        StubDataProvider::new()
            .with_token(vec![0, 1, 2], "test_env".to_string(), Token::new().with_id(vec![0, 1, 2])),
    );
    let finalizer = make_finalizer_with_registry_and_data_provider(
        node_registry,
        make_test_pending_registry(),
        data_provider,
    );

    // An authenticated, registered executor's honest first signature. The
    // pre-fix code trusted a self-declared `public_key` with an empty
    // envelope signature; Phase 1.4 requires the envelope signature to verify
    // and the voter to be registered as an Executor. The real intent of this
    // test — optimistic finalize with a real previous_hash lookup against a
    // seeded token — is unchanged.
    let message = build_signed_sign_message("test_env", b"test_tx_001", vec![1, 2, 3], 10, &voter);

    // First signature → optimistic finalize, now with a real previous_hash
    // lookup against the seeded token
    assert!(finalizer.handle_signature(&message).await.is_ok());
}
