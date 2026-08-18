//! Offline integration tests for the RNS transport boundary.
//!
//! These tests exercise `Gossiper::send_to_type` and `NodeRegistry` through a
//! test-only `Sender` implementation, so no live Reticulum network is required.

use pneumatic_core::config::Config;
use pneumatic_core::conns::ConnError;
use pneumatic_core::conns::senders::Sender;
use pneumatic_core::crypto::{AsymCryptoProvider, Ed25519Provider};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::rns::identity::NodeIdentity;
use std::sync::{Arc, Mutex, RwLock};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

/// A Connection that discards all data sent over it.
/// It uses a real TCP socket pair so it satisfies the Connection trait
/// without needing to implement async methods directly.
struct DiscardConnection {
    stream: Arc<Mutex<TcpStream>>,
}

impl DiscardConnection {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Spawn a background thread to discard incoming data
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = loop {
                    let mut buf = [0u8; 1024];
                    match stream.read(&mut buf) {
                        Ok(0) => break, // EOF
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                };
            }
        });
        let stream = TcpStream::connect(addr).unwrap();
        DiscardConnection { stream: Arc::new(Mutex::new(stream)) }
    }
}

#[async_trait::async_trait]
impl pneumatic_core::conns::Connection for DiscardConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), ConnError> {
        let mut stream = self.stream.lock().unwrap();
        let _ = stream.write_all(data);
        Ok(())
    }
}

/// A recording sender that captures each (rhash, data) pair and returns Ok.
#[derive(Clone)]
struct RecordingSender {
    recorder: Arc<Mutex<Vec<(Vec<u8>, [u8; 16])>>>,
    rhash: [u8; 16],
}

impl Sender for RecordingSender {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        self.recorder.lock().unwrap().push((data.to_vec(), self.rhash));
        Ok(vec![])
    }
}

/// A sender that fails for specific rhash values.
struct ScriptedSender {
    recorder: Arc<Mutex<Vec<(Vec<u8>, [u8; 16])>>>,
    rhash: [u8; 16],
    failing_rhash: Option<[u8; 16]>,
}

impl Sender for ScriptedSender {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        if let Some(fr) = self.failing_rhash {
            if self.rhash == fr {
                return Err(ConnError::IO("transport failure".to_string()));
            }
        }
        self.recorder.lock().unwrap().push((data.to_vec(), self.rhash));
        Ok(vec![])
    }
}

fn make_gossiper() -> (Gossiper, Arc<RwLock<dyn AsymCryptoProvider>>) {
    let config = make_test_config();
    let crypto_provider: Arc<RwLock<dyn AsymCryptoProvider>> =
        Arc::new(RwLock::new(Ed25519Provider::generate()));
    let gossiper = Gossiper::new(
        NodeRegistryType::Committer,
        config,
        300,
        crypto_provider.clone(),
    );
    (gossiper, crypto_provider)
}

fn make_test_config() -> Config {
    use pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT;
    use pneumatic_core::node::NodeType;
    use pneumatic_core::environment::{EnvironmentMetadata, CostModel};
    use pneumatic_core::validation::{ValidationSpecRegistry, BlockValidatorSpecRegistry};

    let identity = NodeIdentity::generate_in_memory();
    let rhash = identity.rhash;
    let public_key = identity.ed25519.public_key().unwrap_or_default();
    let env_metadata = EnvironmentMetadata {
        environment_id: "test".to_string(),
        environment_name: "test".to_string(),
        token_partition_id: "token".to_string(),
        contract_partition_id: None,
        proxy_auth_partition_id: None,
        slush_partition_id: "slush".to_string(),
        partitions: vec![],
        quorum_percentage: 67.0,
        override_quorum_percentage: 67.0,
        max_risk: 1.0,
        cost_model: CostModel::default(),
        asym_crypto_provider: Arc::new(RwLock::new(Ed25519Provider::generate())),
        block_validators: Arc::new(dashmap::DashMap::new()),
        transaction_validation_specs: Arc::new(ValidationSpecRegistry::new()),
        block_validator_specs: Arc::new(BlockValidatorSpecRegistry::new()),
        logger: Arc::new(pneumatic_core::logging::FileLogger::new("test.log".to_string())),
        allowed_token_types: vec![],
        sym_crypto_provider: "aes256-gcm".to_string(),
        serialization_provider: "rmp-serde".to_string(),
        shard_count: 1,
        shard_quorum_percentage: 67.0,
    };
    let mut env_map = dashmap::DashMap::new();
    env_map.insert("test".to_string(), env_metadata);
    Config {
        public_key,
        ip_address: "127.0.0.1".parse().unwrap(),
        rest_api_version: 1,
        node_type: NodeType::Full,
        node_registry_types: vec![NodeRegistryType::Committer],
        main_environment_id: "test".to_string(),
        reconciliation_partition_id: "recon".to_string(),
        environment_metadata: Arc::new(env_map),
        type_configs: Arc::new(dashmap::DashMap::new()),
        identity: Arc::new(identity),
        rhash,
        bootstrap_peers: Vec::new(),
        rns_port: DEFAULT_UDP_PORT,
        transport_enabled: false,
    }
}

fn make_registry(_config: &Config) -> NodeRegistry {
    // Use a config with environment metadata for node registration
    let env_config = make_config_with_env();
    println!("Config main_environment_id: {}", env_config.main_environment_id);
    NodeRegistry::init(
        Arc::new(env_config),
        None,
        Arc::new(|_, _| true),
    )
}

/// Create a Config with environment metadata for node registration.
fn make_config_with_env() -> Config {
    use pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT;
    use pneumatic_core::node::NodeType;
    use pneumatic_core::environment::{EnvironmentMetadata, CostModel};
    use pneumatic_core::validation::{ValidationSpecRegistry, BlockValidatorSpecRegistry};

    let identity = NodeIdentity::generate_in_memory();
    let rhash = identity.rhash;
    let public_key = identity.ed25519.public_key().unwrap_or_default();
    let env_metadata = EnvironmentMetadata {
        environment_id: "test".to_string(),
        environment_name: "test".to_string(),
        token_partition_id: "token".to_string(),
        contract_partition_id: None,
        proxy_auth_partition_id: None,
        slush_partition_id: "slush".to_string(),
        partitions: vec![],
        quorum_percentage: 67.0,
        override_quorum_percentage: 67.0,
        max_risk: 1.0,
        cost_model: CostModel::default(),
        asym_crypto_provider: Arc::new(RwLock::new(Ed25519Provider::generate())),
        block_validators: Arc::new(dashmap::DashMap::new()),
        transaction_validation_specs: Arc::new(ValidationSpecRegistry::new()),
        block_validator_specs: Arc::new(BlockValidatorSpecRegistry::new()),
        logger: Arc::new(pneumatic_core::logging::FileLogger::new("test.log".to_string())),
        allowed_token_types: vec![],
        sym_crypto_provider: "aes256-gcm".to_string(),
        serialization_provider: "rmp-serde".to_string(),
        shard_count: 1,
        shard_quorum_percentage: 67.0,
    };
    let mut env_map = dashmap::DashMap::new();
    env_map.insert("test".to_string(), env_metadata);
    let mut type_configs = dashmap::DashMap::new();
    type_configs.insert(NodeRegistryType::Committer.clone(), pneumatic_core::node::NodeTypeConfig {
        min: 1,
        max: 10,
        min_stake: 0,
    });
    Config {
        public_key,
        ip_address: "127.0.0.1".parse().unwrap(),
        rest_api_version: 1,
        node_type: NodeType::Full,
        node_registry_types: vec![NodeRegistryType::Committer],
        main_environment_id: "test".to_string(),
        reconciliation_partition_id: "recon".to_string(),
        environment_metadata: Arc::new(env_map),
        type_configs: Arc::new(type_configs),
        identity: Arc::new(identity),
        rhash,
        bootstrap_peers: Vec::new(),
        rns_port: DEFAULT_UDP_PORT,
        transport_enabled: false,
    }
}

fn register_node(registry: &NodeRegistry, node_type: &NodeRegistryType, identity: &NodeIdentity) {
    let public_key = identity.ed25519.public_key().unwrap();
    println!("public_key length: {}, rhash: {:?}", public_key.len(), identity.rhash);
    let success = registry.register_peer(
        public_key,
        identity.rhash,
        node_type,
        Box::new(DiscardConnection::new()),
    );
    if !success {
        eprintln!("Registration failed");
    }
}

#[test]
fn test_fanout_records_all_peers() {
    let (gossiper, _crypto) = make_gossiper();
    let config = Config::new_for_testing(
        "test".to_string(),
        Arc::new(dashmap::DashMap::new()),
        Arc::new(dashmap::DashMap::new()),
    );
    let registry = make_registry(&config);
    let identity_a = NodeIdentity::generate_in_memory();
    let identity_b = NodeIdentity::generate_in_memory();
    let a_rhash = identity_a.rhash;
    let b_rhash = identity_b.rhash;

    register_node(&registry, &NodeRegistryType::Committer, &identity_a);
    register_node(&registry, &NodeRegistryType::Committer, &identity_b);

    let recorder = Arc::new(Mutex::new(Vec::new()));
    let payload = vec![1u8, 2, 3];
    gossiper
        .send_to_type(
            &registry,
            &NodeRegistryType::Committer,
            |node| RecordingSender {
                recorder: recorder.clone(),
                rhash: node.rhash,
            },
            &payload,
        )
        .unwrap();

    let recorded = recorder.lock().unwrap();
    assert_eq!(recorded.len(), 2);
    let mut rhashes: Vec<[u8; 16]> = recorded.iter().map(|(_, r)| *r).collect();
    rhashes.sort();
    let mut expected = vec![a_rhash, b_rhash];
    expected.sort();
    assert_eq!(rhashes, expected);
    for (data, _) in recorded.iter() {
        assert_eq!(data, &payload);
    }
}

#[test]
fn test_type_filtering() {
    let (gossiper, _crypto) = make_gossiper();
    let config = Config::new_for_testing(
        "test".to_string(),
        Arc::new(dashmap::DashMap::new()),
        Arc::new(dashmap::DashMap::new()),
    );
    let registry = make_registry(&config);
    let identity_a = NodeIdentity::generate_in_memory();
    let identity_b = NodeIdentity::generate_in_memory();

    register_node(&registry, &NodeRegistryType::Committer, &identity_a);
    register_node(&registry, &NodeRegistryType::Committer, &identity_b);

    let recorder = Arc::new(Mutex::new(Vec::new()));
    let payload = vec![1u8];
    // Send to Committers only; Sentinel has no registered nodes.
    gossiper
        .send_to_type(
            &registry,
            &NodeRegistryType::Sentinel,
            |node| RecordingSender {
                recorder: recorder.clone(),
                rhash: node.rhash,
            },
            &payload,
        )
        .unwrap();

    assert!(recorder.lock().unwrap().is_empty());
}

#[test]
fn test_empty_registry_is_ok() {
    let (gossiper, _crypto) = make_gossiper();
    let config = Config::new_for_testing(
        "test".to_string(),
        Arc::new(dashmap::DashMap::new()),
        Arc::new(dashmap::DashMap::new()),
    );
    let registry = make_registry(&config);

    let recorder = Arc::new(Mutex::new(Vec::new()));
    let result = gossiper.send_to_type(
        &registry,
        &NodeRegistryType::Committer,
        |node| RecordingSender {
            recorder: recorder.clone(),
            rhash: node.rhash,
        },
        &[1, 2, 3],
    );

    assert!(result.is_ok());
    assert!(recorder.lock().unwrap().is_empty());
}

#[test]
fn test_partial_transport_failure() {
    let (gossiper, _crypto) = make_gossiper();
    let config = Config::new_for_testing(
        "test".to_string(),
        Arc::new(dashmap::DashMap::new()),
        Arc::new(dashmap::DashMap::new()),
    );
    let registry = make_registry(&config);
    let identity_a = NodeIdentity::generate_in_memory();
    let identity_b = NodeIdentity::generate_in_memory();
    let a_rhash = identity_a.rhash;

    register_node(&registry, &NodeRegistryType::Committer, &identity_a);
    register_node(&registry, &NodeRegistryType::Committer, &identity_b);

    // Sender fails for rhash == a_rhash, succeeds for others.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let result = gossiper.send_to_type(
        &registry,
        &NodeRegistryType::Committer,
        |node| ScriptedSender {
            recorder: recorder.clone(),
            rhash: node.rhash,
            failing_rhash: Some(a_rhash),
        },
        &[1, 2, 3],
    );

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), pneumatic_core::data::DataError::FromStore(_)));
}

#[test]
fn test_total_transport_failure() {
    let (gossiper, _crypto) = make_gossiper();
    let config = Config::new_for_testing(
        "test".to_string(),
        Arc::new(dashmap::DashMap::new()),
        Arc::new(dashmap::DashMap::new()),
    );
    let registry = make_registry(&config);
    let identity_a = NodeIdentity::generate_in_memory();
    register_node(&registry, &NodeRegistryType::Committer, &identity_a);

    // All senders fail.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let result = gossiper.send_to_type(
        &registry,
        &NodeRegistryType::Committer,
        |node| ScriptedSender {
            recorder: recorder.clone(),
            rhash: node.rhash,
            failing_rhash: Some(node.rhash), // Fail for all
        },
        &[1, 2, 3],
    );

    assert!(result.is_err());
}

#[test]
fn test_concurrent_publication() {
    let (gossiper, _crypto) = make_gossiper();
    let config = Config::new_for_testing(
        "test".to_string(),
        Arc::new(dashmap::DashMap::new()),
        Arc::new(dashmap::DashMap::new()),
    );
    let registry = make_registry(&config);
    let identity_a = NodeIdentity::generate_in_memory();
    register_node(&registry, &NodeRegistryType::Committer, &identity_a);

    let gossiper = Arc::new(gossiper);
    let registry = Arc::new(registry);
    let recorder = Arc::new(Mutex::new(Vec::new()));

    let mut handles = Vec::new();
    for _ in 0..5 {
        let g = Arc::clone(&gossiper);
        let reg = Arc::clone(&registry);
        let rec = Arc::clone(&recorder);
        handles.push(thread::spawn(move || {
            let payload = vec![0xABu8];
            g.send_to_type(
                &reg,
                &NodeRegistryType::Committer,
                |node| RecordingSender {
                    recorder: rec.clone(),
                    rhash: node.rhash,
                },
                &payload,
            )
            .unwrap();
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(recorder.lock().unwrap().len(), 5);
}