pub use std::sync::{Arc, Mutex};

pub use dashmap::DashMap;
pub use pneumatic_core::config::Config;
pub use pneumatic_core::crypto::AsymCryptoProvider;
pub use pneumatic_core::rns::identity::NodeIdentity;
pub use pneumatic_core::node::{NodeRegistryRequest, NodeRegistryType, NodeType, NodeTypeConfig};
pub use pneumatic_core::user::User;
pub use pneumatic_core::conns::ConnError;
pub use pneumatic_core::data::{DataError, DefaultDataProvider, StubDataProvider};
pub use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
pub use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
pub use pneumatic_core::gossiper::Gossiper;
pub use pneumatic_core::messages::Message;
pub use pneumatic_core::registry::{NullifierRegistry, PendingTransactionRegistry};
pub use pneumatic_core::shielded::{
    commit, nullifier, root_to_bytes, IncrementalMerkleTree, MerkleRootState, ShieldedNote,
    SimpleShieldedPoolView, ShieldedPoolView, DEFAULT_DEPTH,
};
pub use pneumatic_core::tokens::{Token, TokenFactory};
pub use pneumatic_core::transactions::{
    PendingTransaction, ShieldedTransaction, Transaction, TransactionState, TransactionValidationResult,
};
pub use pneumatic_core::errors::{PneumaticError, TransactionRiskFactor, ValidationFailureReason};
pub use pneumatic_core::validation::{
    SelfSignedBlockValidatorSpec, ShieldedValidationDeps, ShieldedValidationSpec,
    TransactionValidationSpec, ValidationSpecRegistry,
};
pub use crate::sentinel::Sentinel;

pub use crate::transaction_notifier::TransactionNotifier;
pub use crate::TransactionValidator;
use super::super::*;

// --- helpers ---

pub fn make_test_config() -> Config {
    let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let public_key = identity.ed25519.public_key().unwrap_or_default();
    let rhash = identity.rhash;
    Config {
        public_key,
        ip_address: "127.0.0.1".parse().unwrap(),
        rest_api_version: 1,
        node_type: NodeType::Full,
        node_registry_types: vec![NodeRegistryType::Committer],
        main_environment_id: "test".to_string(),
        reconciliation_partition_id: "recon".to_string(),
        environment_metadata: Arc::new(DashMap::new()),
        // Node-type capacity entries are required — without them get_max_node_number
        // returns 0 and register_peer rejects every peer. The epoch-advance tests below
        // register a finalizer peer to satisfy the role guard.
        type_configs: Arc::new({
            let tc = DashMap::new();
            for t in [
                NodeRegistryType::Committer,
                NodeRegistryType::Sentinel,
                NodeRegistryType::Executor,
                NodeRegistryType::Finalizer,
                NodeRegistryType::Archiver,
            ] {
                tc.insert(t.clone(), pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 10 });
            }
            tc
        }),
        identity: Arc::new(identity),
        rhash,
        bootstrap_peers: Vec::new(),
        rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
        transport_enabled: false,
    }
}


pub fn make_test_node_registry() -> Arc<NodeRegistry> {
    Arc::new(NodeRegistry::init(
        Arc::new(make_test_config()),
        None,
        Arc::new(|_, _| true),
    ))
}


pub fn make_test_env_data() -> EnvironmentMetadata {
    let json = r#"{"environment_id":"test","environment_name":"test",
        "partitions":[{"id":"token","partition_type":"Token"},
        {"id":"slush","partition_type":"Slush"}],
        "asym_crypto_provider":{"Ed25519":null},"sym_crypto_provider":"sym",
        "serialization_provider":"rmp","quorum_percentage":67.0,
        "override_quorum_percentage":0.0,"max_risk":1.0,
        "allowed_token_types":[],"trans_validation_specs":[],
        "block_validation_specs":[],"log_file":"test.log"}"#;
    let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).unwrap();
    EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec")
}


/// Same as `make_test_env_data` but with shard-aware routing enabled, for
/// tests that exercise the per-shard executor selection path.
pub fn make_test_env_data_sharded() -> EnvironmentMetadata {
    let json = r#"{"environment_id":"test","environment_name":"test",
        "partitions":[{"id":"token","partition_type":"Token"},
        {"id":"slush","partition_type":"Slush"}],
        "asym_crypto_provider":{"Ed25519":null},"sym_crypto_provider":"sym",
        "serialization_provider":"rmp","quorum_percentage":67.0,
        "override_quorum_percentage":0.0,"max_risk":1.0,
        "allowed_token_types":[],"trans_validation_specs":[],
        "block_validation_specs":[],"log_file":"test.log",
        "shard_count":2}"#;
    let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).unwrap();
    EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec")
}


pub fn make_sentinel_fixture() -> (Sentinel, Arc<PendingTransactionRegistry>) {
    make_sentinel_fixture_with_data_provider(StubDataProvider::new())
}


pub fn make_sentinel_fixture_with_data_provider(
    data_provider: StubDataProvider,
) -> (Sentinel, Arc<PendingTransactionRegistry>) {
    make_sentinel_fixture_with_env_and_data_provider(data_provider, make_test_env_data())
}


/// Shared fixture build for a custom environment (e.g. shard-aware routing).
pub fn make_sentinel_fixture_with_env_and_data_provider(
    data_provider: StubDataProvider,
    env_data: EnvironmentMetadata,
) -> (Sentinel, Arc<PendingTransactionRegistry>) {
    make_sentinel_fixture_with_env_data_and_view(
        data_provider,
        env_data,
        // Phase S5.1: the default fixture gets a pristine pool view —
        // every pre-existing test keeps exactly its S5.1-prior behavior
        // (a pristine view fails closed on its own: any non-genesis-root
        // transfer would be a StaleMerkleRoot, any nullifier unspent).
        Arc::new(SimpleShieldedPoolView::new(10)),
    )
}


/// Same build with an explicit pool view (the S5.1 shielded-handler
/// fixtures: stale-state and live-proof views built over pre-shared
/// `NullifierRegistry`/`MerkleRootState`).
pub fn make_sentinel_fixture_with_env_data_and_view(
    data_provider: StubDataProvider,
    env_data: EnvironmentMetadata,
    pool_view: Arc<dyn ShieldedPoolView>,
) -> (Sentinel, Arc<PendingTransactionRegistry>) {
    let registry = Arc::new(PendingTransactionRegistry::new());
    let node_registry = make_test_node_registry();
    let env_data = Arc::new(env_data);
    let gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Sentinel,
        make_test_config(),
        300,
        env_data.asym_crypto_provider.clone(),
    ));
    let sentinel = Sentinel::new(
        make_test_config(),
        env_data,
        node_registry,
        registry.clone(),
        gossiper,
        Arc::new(data_provider),
        pool_view,
    );
    (sentinel, registry)
}


/// Sentinel fixture with the Sentinel-type registration floor overridden to
/// `type_min` (the global floor stays at the default 10, since the test env
/// is empty). Decoupling the per-type floor from the global floor lets the
/// AND semantics of `check_stake_for_type` be exercised independently —
/// AUDIT Phase 4.4.
pub fn make_sentinel_fixture_sentinel_floor(
    type_min: u64,
    data_provider: StubDataProvider,
) -> (Sentinel, Arc<NodeRegistry>) {
    let config = make_test_config();
    config
        .type_configs
        .get_mut(&NodeRegistryType::Sentinel)
        .expect("Sentinel is a supported registry type")
        .min_stake = type_min;
    let config_arc = Arc::new(config.clone());
    let registry = Arc::new(NodeRegistry::init(
        config_arc,
        None,
        Arc::new(|_, _| true),
    ));
    let env_data = Arc::new(make_test_env_data());
    let gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Sentinel,
        make_test_config(),
        300,
        env_data.asym_crypto_provider.clone(),
    ));
    let sentinel = Sentinel::new(
        config,
        env_data,
        registry.clone(),
        Arc::new(PendingTransactionRegistry::new()),
        gossiper,
        Arc::new(data_provider),
        Arc::new(SimpleShieldedPoolView::new(10)),
    );
    (sentinel, registry)
}


// --- handle_confirmation tests ---

pub fn make_finalizing_entry(registry: &PendingTransactionRegistry, tx_id: &str, finalizer_key: Vec<u8>) {
    registry.register_pending(tx_id.into()).unwrap();
    if let Ok(mut entry) = registry.get_transaction_mut(tx_id) {
        entry.transition_to_validated(
            Transaction {
                id: tx_id.into(), action: "Transfer".into(),
                token_id: vec![1], bid: None, sequence_number: 1,
                sender: vec![1], receiver: vec![2], amount: Some(100),
                timestamp: 0, result_hash: vec![],
                sender_signature: vec![],
            },
            TransactionValidationResult::valid(
                finalizer_key.clone(),
                TransactionRiskFactor {
                    affected_parties: 2, amount: 100,
                    is_contract: false, is_multi_party: false,
                },
            ),
        );
    }
    registry.set_requested_finalizer(tx_id, finalizer_key).unwrap();
}


// -----------------------------------------------------------------------
// Phase 5.3 (AUDIT H3): deterministic finalizer / shard routing must bind
// the mined chain tip (prev_block_hash) so the assignment is unpredictable
// until the tip is produced. The seed is a deterministic function of (tip,
// epoch, tx_id), so comparing per-tx assignments between two different tips
// is deterministic and non-flaky — it fails iff the tip never reaches the
// seed.
// -----------------------------------------------------------------------

/// Build a token whose blockchain holds a single internally-consistent block
/// (so its tip is that block's hash, non-empty).
pub fn token_with_one_block() -> Token {
    let mut token = Token::new();
    token.id = vec![1];
    token.environment_id = "test".to_string();
    let mut block = Block {
        signed_trans: pneumatic_core::transactions::SignedTransaction::test_transaction(),
        token_metadata: std::collections::HashMap::new(),
        previous_hash: vec![], // genesis convention
        timestamp: 0,
        current_hash: vec![],
        finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block)
        .expect("well-formed test block hash");
    token.blockchain.add_block(block);
    token
}

