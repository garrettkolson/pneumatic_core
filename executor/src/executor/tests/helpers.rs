//! Shared fixtures for the executor test suite (committer convention:
//! env/config/registry builders + the recording connection and the
//! envelope-assert helper, pub-ified, re-exporting the module header's imports).

use super::super::*;
pub use pneumatic_core::config::Config;
pub use pneumatic_core::crypto::{AsymCryptoProvider, BasicHashProvider};
pub use pneumatic_core::data::DefaultDataProvider;
pub use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
pub use pneumatic_core::node::{NodeRegistryType, NodeType};
pub use pneumatic_core::transactions::{PendingTransaction, TransactionState};

pub fn make_test_hash_provider() -> Arc<dyn HashProvider> {
    Arc::new(BasicHashProvider::new())
}

pub fn make_test_env_data() -> Arc<DashMap<String, EnvironmentMetadata>> {
    let env_map = DashMap::new();
    let spec_json = r#"{
        "environment_id": "test_env",
        "main_token_partition_id": "token",
        "reconciliation_partition_id": "reconciliation",
        "quorum_percentage": 67,
        "security_level": 2,
        "chain_count": 2,
        "node_registry_type": 0,
        "max_stake": 0,
        "min_stake": 0,
        "crypto_provider": "BasicHashProvider",
        "sym_crypto_provider": "AES",
        "serialization_provider": "MsgPack",
        "blockchain_metadata": [],
        "block_validators": [],
        "data_provider": "DefaultDataProvider",
        "rest_api_version": 1,
        "is_full_node": true,
        "is_light_node": false,
        "max_in_flight": 100,
        "max_gas_limit": 1000000,
        "max_risk": 1.0,
        "allowed_token_types": [],
        "trans_validation_specs": [],
        "block_validation_specs": [],
        "logger": "FileLogger"
    }"#;
    if let Ok(spec) = serde_json::from_str::<EnvironmentMetadataSpec>(spec_json) {
        let env = EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec");
        env_map.insert(env.environment_id.clone(), env);
    }
    Arc::new(env_map)
}

pub fn make_test_config() -> Config {
    let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let public_key = identity.ed25519.public_key().unwrap_or_default();
    let rhash = identity.rhash;
    Config {
        public_key,
        ip_address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        rest_api_version: 1,
        node_type: NodeType::Full,
        node_registry_types: vec![NodeRegistryType::Executor],
        main_environment_id: "test_env".to_string(),
        reconciliation_partition_id: "reconciliation".to_string(),
        environment_metadata: make_test_env_data(),
        // Capacity entries are required — without them get_max_node_number
        // returns 0 and register_peer rejects every peer.
        type_configs: Arc::new({
            let tc = DashMap::new();
            tc.insert(NodeRegistryType::Finalizer.clone(),
                pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 });
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
    let config = make_test_config();
    Arc::new(NodeRegistry::init(
        Arc::new(config),
        None,
        Arc::new(|_, _| true),
    ))
}

pub fn make_test_pending_registry() -> Arc<PendingTransactionRegistry> {
    let registry = Arc::new(PendingTransactionRegistry::new());
    let tx = Transaction {
        id: "test_tx_001".to_string(),
        action: "Transfer".to_string(),
        token_id: vec![0, 1, 2],
        bid: None,
        sequence_number: 1,
        sender: vec![10, 20, 30],
        receiver: vec![40, 50, 60],
        amount: Some(100),
        timestamp: 1000,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let pending = PendingTransaction::new("test_tx_001".to_string(), TransactionState::Preloaded { transaction: tx });
    let _ = registry.add_transaction("test_tx_001".to_string(), pending);
    registry
}

pub fn make_test_data_provider() -> Arc<dyn DataProvider> {
    Arc::new(DefaultDataProvider::new())
}

// -----------------------------------------------------------------------
// Phase 1.1 regression: Execute dispatch signed with node identity
// -----------------------------------------------------------------------

/// A Connection that records each sent payload verbatim.
pub struct RecordingConnection {
    pub recorder: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl pneumatic_core::conns::Connection for RecordingConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), pneumatic_core::conns::ConnError> {
        self.recorder.lock().unwrap().push(data.clone());
        Ok(())
    }
}

/// Assert the message envelope is signed by `identity` — the same check
/// the gossiper performs: signature over `body` under `public_key`.
pub fn assert_signed_by(
    message: &pneumatic_core::messages::Message,
    identity: &pneumatic_core::rns::identity::NodeIdentity,
) {
    let expected_pk = identity.ed25519.public_key().expect("identity pubkey");
    assert_eq!(
        message.public_key, expected_pk,
        "message.public_key must be the sender's identity key"
    );
    let verifier = pneumatic_core::crypto::Ed25519Provider::generate();
    let ok = verifier
        .check_signature(&message.signature, &message.public_key, &message.body)
        .expect("signature check should succeed");
    assert!(ok, "message body must verify under the sender's identity key");
}
