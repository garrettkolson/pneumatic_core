use std::sync::Arc;

use pneumatic_core::config::Config;
use pneumatic_core::conns::ConnError;
use pneumatic_core::data::DataError;
use pneumatic_core::encoding::serialize_to_bytes_rmp;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::messages::Message;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::transactions::Transaction;

/// Handles all outbound message sending from the Sentinel to other node types.
/// Packages transactions and commands into the proper `Message` wire format.
pub struct TransactionNotifier {
    config: Config,
    node_registry: Arc<NodeRegistry>,
}

impl TransactionNotifier {
    pub fn new(config: Config, node_registry: Arc<NodeRegistry>) -> Self {
        TransactionNotifier { config, node_registry }
    }

    /// Send a transaction to all Executors for data preloading.
    pub fn send_to_executors_for_preload(
        &self,
        tx: &Transaction,
        env: &EnvironmentMetadata,
    ) -> Result<(), NotifyError> {
        let body = serialize_to_bytes_rmp(tx).map_err(NotifyError::Encoding)?;
        let msg = Message::signed(
            env.token_partition_id.clone(),
            "Preload",
            body,
            None,
            &self.config.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        self.send_to_nodes(NodeRegistryType::Executor, payload)
    }

    /// Send a transaction to specific Executors for data preloading (shard-aware).
    pub fn send_to_shard_executors_for_preload(
        &self,
        tx: &Transaction,
        executor_keys: &[Vec<u8>],
        env: &EnvironmentMetadata,
    ) -> Result<(), NotifyError> {
        let body = serialize_to_bytes_rmp(tx).map_err(NotifyError::Encoding)?;
        let msg = Message::signed(
            env.token_partition_id.clone(),
            "Preload",
            body,
            None,
            &self.config.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        let registry = Arc::clone(&self.node_registry);
        let payload_clone = payload.clone();

        // Send to each executor key in the shard
        for key in executor_keys {
            let key_clone = key.clone();
            let payload_inner = payload_clone.clone();
            let reg = Arc::clone(&registry);
            let _ = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("could not build mini runtime for shard preload send");
                rt.block_on(async {
                    let Some(nodes) = reg.get_nodes(&NodeRegistryType::Executor) else { return };
                    if let Some(entry) = nodes.get(&key_clone) {
                        let _ = entry.value().conn.send(&payload_inner).await;
                    };
                });
            });
        }
        Ok(())
    }

    /// Send a validated transaction to the assigned Finalizer for execution preloading.
    pub fn send_to_finalizer_for_preload(
        &self,
        tx: &Transaction,
        finalizer_key: &[u8],
        env: &EnvironmentMetadata,
    ) -> Result<(), NotifyError> {
        let body = serialize_to_bytes_rmp(tx).map_err(NotifyError::Encoding)?;
        // Sign with our own identity — the receiver verifies against the
        // sender's registered key, never the destination's.
        let msg = Message::signed(
            env.token_partition_id.clone(),
            "Preload",
            body,
            None,
            &self.config.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        self.send_to_nodes(NodeRegistryType::Finalizer, payload)
    }

    /// Notify all Sentinels to continue processing a cleared transaction.
    pub fn notify_clear_to_process(&self, tx_id: &str, env: &EnvironmentMetadata) -> Result<(), NotifyError> {
        let body = serialize_to_bytes_rmp(&tx_id).map_err(NotifyError::Encoding)?;
        let msg = Message::signed(
            env.token_partition_id.clone(),
            "Clear",
            body,
            None,
            &self.config.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        self.send_to_nodes(NodeRegistryType::Sentinel, payload)
    }

    /// Notify all Sentinels to delete a transaction from the registry.
    pub fn notify_delete(&self, tx_id: &str, env: &EnvironmentMetadata) -> Result<(), NotifyError> {
        let body = serialize_to_bytes_rmp(&tx_id).map_err(NotifyError::Encoding)?;
        let msg = Message::signed(
            env.token_partition_id.clone(),
            "Delete",
            body,
            None,
            &self.config.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        self.send_to_nodes(NodeRegistryType::Sentinel, payload)
    }

    /// Request a Finalizer to take responsibility for a validated transaction.
    pub fn request_finalizer(
        &self,
        tx: &Transaction,
        env: &EnvironmentMetadata,
    ) -> Result<Vec<u8>, NotifyError> {
        let body = serialize_to_bytes_rmp(tx).map_err(NotifyError::Encoding)?;
        let msg = Message::signed(
            env.token_partition_id.clone(),
            "FinalizerRequest",
            body,
            None,
            &self.config.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        let _ = self.send_to_nodes(NodeRegistryType::Finalizer, payload)?;
        Ok(vec![])
    }

    /// Request a specific Finalizer (by key) to take responsibility for a validated transaction.
    pub fn request_single_finalizer(
        &self,
        tx: &Transaction,
        finalizer_key: Vec<u8>,
        env: &EnvironmentMetadata,
    ) -> Result<(), NotifyError> {
        let body = serialize_to_bytes_rmp(tx).map_err(NotifyError::Encoding)?;
        let msg = Message::signed(
            env.token_partition_id.clone(),
            "FinalizerRequest",
            body,
            None,
            &self.config.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        let registry = Arc::clone(&self.node_registry);
        let payload_clone = payload.clone();
        let finalizer_key_clone = finalizer_key.clone();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("could not build mini runtime for request_single_finalizer");
            rt.block_on(async {
                let Some(nodes) = registry.get_nodes(&NodeRegistryType::Finalizer) else { return };
                if let Some(entry) = nodes.get(&finalizer_key_clone) {
                    let _ = entry.value().conn.send(&payload_clone).await;
                };
            });
        });
        Ok(())
    }

    /// Broadcast a message to all nodes of a given type in the registry.
    fn send_to_nodes(&self, target_type: NodeRegistryType, payload: Vec<u8>) -> Result<(), NotifyError> {
        let registry = Arc::clone(&self.node_registry);
        let payload_clone = payload.clone();
        // Spawn a bare OS thread that creates its own mini Tokio runtime
        // to drive the async send_to_all. Works inside or outside any
        // existing Tokio reactor context.
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("could not build mini runtime for send_to_nodes");
            rt.block_on(registry.send_to_all(payload_clone, &target_type));
        });
        Ok(())
    }
}

/// Errors specific to transaction notification/sending.
#[derive(Debug)]
pub enum NotifyError {
    Encoding(std::io::Error),
    Connection(ConnError),
    Data(DataError),
    NoTarget(NodeRegistryType),
}

impl From<ConnError> for NotifyError {
    fn from(e: ConnError) -> Self {
        NotifyError::Connection(e)
    }
}

impl From<DataError> for NotifyError {
    fn from(e: DataError) -> Self {
        NotifyError::Data(e)
    }
}

impl From<PneumaticError> for NotifyError {
    fn from(e: PneumaticError) -> Self {
        NotifyError::Data(DataError::CryptoError(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use pneumatic_core::crypto::{AsymCryptoProvider, Ed25519Provider};
    use pneumatic_core::encoding::deserialize_rmp_to;
    use pneumatic_core::environment::{CostModel, EnvironmentMetadata};
    use pneumatic_core::logging::FileLogger;
    use pneumatic_core::node::{NodeType, NodeRegistryType};
    use pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT;
    use pneumatic_core::rns::identity::NodeIdentity;
    use pneumatic_core::validation::{BlockValidatorSpecRegistry, ValidationSpecRegistry};
    use std::sync::{Arc, Mutex, RwLock};

    /// A Connection that records each sent payload verbatim.
    struct RecordingConnection {
        recorder: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait::async_trait]
    impl pneumatic_core::conns::Connection for RecordingConnection {
        async fn send(&self, data: &Vec<u8>) -> Result<(), pneumatic_core::conns::ConnError> {
            self.recorder.lock().unwrap().push(data.clone());
            Ok(())
        }
    }

    /// `send_to_nodes` spawns a detached thread, so poll the recorder
    /// until the expected number of payloads arrive (or the deadline lapses).
    fn poll_recorder(recorder: &Arc<Mutex<Vec<Vec<u8>>>>, expected: usize) -> bool {
        for _ in 0..100 {
            if recorder.lock().unwrap().len() >= expected {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    /// Assert the message envelope is signed by `identity` — the same check
    /// the gossiper performs: signature over `body` under `public_key`.
    fn assert_signed_by(message: &Message, identity: &NodeIdentity) {
        let expected_pk = identity.ed25519.public_key().expect("identity pubkey");
        assert_eq!(
            message.public_key, expected_pk,
            "message.public_key must be the sender's identity key"
        );
        let verifier = Ed25519Provider::generate();
        let ok = verifier
            .check_signature(&message.signature, &message.public_key, &message.body)
            .expect("signature check should succeed");
        assert!(ok, "message body must verify under the sender's identity key");
    }

    fn make_notifier() -> (TransactionNotifier, Arc<NodeIdentity>, Arc<NodeRegistry>) {
        let identity = Arc::new(NodeIdentity::generate_in_memory());
        let public_key = identity.ed25519.public_key().unwrap_or_default();
        let rhash = identity.rhash;
        let config = Config {
            public_key,
            ip_address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Sentinel],
            main_environment_id: "test_env".to_string(),
            reconciliation_partition_id: "reconciliation".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            type_configs: Arc::new(DashMap::new()),
            identity: identity.clone(),
            rhash,
            bootstrap_peers: Vec::new(),
            rns_port: DEFAULT_UDP_PORT,
            transport_enabled: false,
        };
        // Capacity entries are required — without them get_max_node_number
        // returns 0 and register_peer rejects every peer.
        let type_configs = DashMap::new();
        type_configs.insert(
            NodeRegistryType::Sentinel.clone(),
            pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 },
        );
        type_configs.insert(
            NodeRegistryType::Finalizer.clone(),
            pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 },
        );
        let config = Config {
            type_configs: Arc::new(type_configs),
            ..config
        };
        let node_registry = Arc::new(NodeRegistry::init(
            Arc::new(config.clone()),
            None,
            Arc::new(|_, _| true),
        ));
        (
            TransactionNotifier::new(config, node_registry.clone()),
            identity,
            node_registry,
        )
    }

    fn make_test_env() -> EnvironmentMetadata {
        EnvironmentMetadata {
            environment_id: "test_env".to_string(),
            environment_name: "Test".to_string(),
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
            transaction_validation_specs: Arc::new(ValidationSpecRegistry::new()),
            block_validator_specs: Arc::new(BlockValidatorSpecRegistry::new()),
            logger: Arc::new(FileLogger::new("test.log".to_string())),
            allowed_token_types: vec![],
            sym_crypto_provider: "aes256-gcm".to_string(),
            serialization_provider: "rmp-serde".to_string(),
            shard_count: 1,
            shard_quorum_percentage: 67.0,
        }
    }

    fn make_test_tx() -> Transaction {
        Transaction {
            id: "tx_001".to_string(),
            action: "Transfer".to_string(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: vec![10, 20],
            receiver: vec![30, 40],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        }
    }

    #[test]
    fn clear_broadcast_is_signed_with_sentinel_identity() {
        let (notifier, identity, registry) = make_notifier();
        let recorder = Arc::new(Mutex::new(Vec::new()));
        assert!(registry.register_peer(
            vec![0xEE; 32],
            [7u8; 16],
            &NodeRegistryType::Sentinel,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ));

        notifier
            .notify_clear_to_process("tx_001", &make_test_env())
            .expect("send should succeed");

        assert!(
            poll_recorder(&recorder, 1),
            "Clear message should reach the sentinel peer"
        );
        let raw = recorder.lock().unwrap()[0].clone();
        let message: Message =
            deserialize_rmp_to(&raw).expect("captured payload should be a Message");
        assert_eq!(message.action, "Clear");
        assert_signed_by(&message, &identity);
    }

    #[test]
    fn preload_to_finalizer_signed_with_sender_not_destination() {
        let (notifier, identity, registry) = make_notifier();
        let recorder = Arc::new(Mutex::new(Vec::new()));
        // The finalizer peer's registered key — the pre-1.1 bug put this
        // destination key into the signature field instead of a signature.
        let finalizer_key = vec![0xAB; 32];
        assert!(registry.register_peer(
            finalizer_key.clone(),
            [8u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ));

        notifier
            .send_to_finalizer_for_preload(&make_test_tx(), &finalizer_key, &make_test_env())
            .expect("send should succeed");

        assert!(
            poll_recorder(&recorder, 1),
            "Preload message should reach the finalizer peer"
        );
        let raw = recorder.lock().unwrap()[0].clone();
        let message: Message =
            deserialize_rmp_to(&raw).expect("captured payload should be a Message");
        assert_eq!(message.action, "Preload");

        // Must verify under the sender (sentinel) identity...
        assert_signed_by(&message, &identity);

        // ...and never under the destination's key.
        let verifier = Ed25519Provider::generate();
        let under_destination = verifier
            .check_signature(&message.signature, &finalizer_key, &message.body)
            .unwrap_or(false);
        assert!(
            !under_destination,
            "signature must not verify under the destination's key"
        );
    }
}
