use std::sync::Arc;

use pneumatic_core::blocks::Block;
use pneumatic_core::encoding::serialize_to_bytes_rmp;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::epoch::StakeSet;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::transactions::TransactionCommit;

// ---------------------------------------------------------------------------
// MessageDispatcher — sends blocks to committers and clears to sentinels
// ---------------------------------------------------------------------------

/// Dispatches committed blocks to Committers and clear notifications to Sentinels.
///
/// This module handles only the networking layer — it does NOT build blocks
/// or check quorum. It takes fully-formed messages and sends them to the
/// appropriate node types.
pub struct MessageDispatcher {
    /// Shared registry of connected nodes
    node_registry: Arc<NodeRegistry>,
    /// Environment ID for routing
    env_id: String,
    /// Public key of this finalizer node
    public_key: Vec<u8>,
    /// Node identity — signs all outgoing dispatch messages
    identity: Arc<NodeIdentity>,
}

impl MessageDispatcher {
    /// Create a new MessageDispatcher.
    pub fn new(
        node_registry: Arc<NodeRegistry>,
        env_id: String,
        public_key: Vec<u8>,
        identity: Arc<NodeIdentity>,
    ) -> Self {
        MessageDispatcher {
            node_registry,
            env_id,
            public_key,
            identity,
        }
    }

    /// Send a TransactionCommit to all Committers in the environment.
    ///
    /// Serializes the commit message and broadcasts it to all nodes
    /// registered as Committers.
    pub async fn send_to_committers(&self, commit: TransactionCommit) -> Result<(), PneumaticError> {
        // Build the message body with the commit data
        let msg_body = serialize_to_bytes_rmp(&commit)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Package as a wire message, signed with this node's identity
        let message = Message::signed(
            self.env_id.clone(),
            "Commit",
            msg_body,
            None,
            &self.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&message)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Broadcast to all committers via registered connections
        self.node_registry.send_to_all(payload, &NodeRegistryType::Committer).await;

        Ok(())
    }

    /// Send a Clear notification to all Sentinels.
    ///
    /// Tells Sentinels to clean up the transaction from their registries
    /// after it has been committed.
    pub async fn send_clear_to_sentinels(&self, tx_id: &str) -> Result<(), PneumaticError> {
        // Serialize the transaction ID
        let msg_body = serialize_to_bytes_rmp(&tx_id)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Package as a wire message, signed with this node's identity
        let message = Message::signed(
            self.env_id.clone(),
            "Clear",
            msg_body,
            None,
            &self.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&message)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Broadcast to all sentinels via registered connections
        self.node_registry.send_to_all(payload, &NodeRegistryType::Sentinel).await;

        Ok(())
    }

    /// Broadcast a BlockFinalized message to all Committers and Archivars.
    ///
    /// Sent after an optimistic commit to inform peer nodes that a block
    /// has been finalized. Includes the stake set (if available) so
    /// receiving nodes can perform stake-weighted confirmation tracking.
    pub async fn send_block_finalized(
        &self,
        block: Block,
        stake_set: Option<StakeSet>,
    ) -> Result<(), PneumaticError> {
        // Serialize the block as the message body
        let msg_body = serialize_to_bytes_rmp(&block)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Package as a wire message, signed with this node's identity
        let message = Message::signed(
            self.env_id.clone(),
            "BlockFinalized",
            msg_body,
            stake_set,
            &self.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&message)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Broadcast to all committers
        self.node_registry
            .send_to_all(payload.clone(), &NodeRegistryType::Committer).await;

        // Broadcast to all archivars
        self.node_registry
            .send_to_all(payload.clone(), &NodeRegistryType::Archiver).await;

        // Broadcast to all sentinels — delivers the finalized block (and its
        // epoch_number + stake_set) so routing nodes can advance to the new epoch.
        self.node_registry
            .send_to_all(payload, &NodeRegistryType::Sentinel).await;

        Ok(())
    }

    /// Broadcast a BlockConfirmed vote from this node.
    ///
    /// Sent by any node that has received and validated a BlockFinalized message.
    /// Contains the block hash and this node's public key as a vote.
    pub async fn send_block_confirmed_vote(&self, block_hash: &[u8]) -> Result<(), PneumaticError> {
        // Serialize block hash + node key as body
        let body = serialize_to_bytes_rmp(&(block_hash.to_vec(), self.public_key.clone()))
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Package as a wire message, signed with this node's identity
        let message = Message::signed(
            self.env_id.clone(),
            "BlockConfirmed",
            body,
            None,
            &self.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&message)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Broadcast to all other nodes of each type
        self.node_registry
            .send_to_all(payload.clone(), &NodeRegistryType::Committer).await;
        self.node_registry
            .send_to_all(payload.clone(), &NodeRegistryType::Archiver).await;
        self.node_registry
            .send_to_all(payload.clone(), &NodeRegistryType::Executor).await;
        self.node_registry
            .send_to_all(payload, &NodeRegistryType::Sentinel).await;

        Ok(())
    }

    /// Broadcast a BlockQuorumReached status update.
    ///
    /// Sent by the node that first reaches quorum, telling all peers
    /// that a block has been confirmed.
    pub async fn send_block_quorum_reached(
        &self,
        block_hash: &[u8],
    ) -> Result<(), PneumaticError> {
        let body = serialize_to_bytes_rmp(&block_hash.to_vec())
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Signed with this node's identity
        let message = Message::signed(
            self.env_id.clone(),
            "BlockQuorumReached",
            body,
            None,
            &self.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&message)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Broadcast to all other node types
        self.node_registry
            .send_to_all(payload.clone(), &NodeRegistryType::Committer).await;
        self.node_registry
            .send_to_all(payload.clone(), &NodeRegistryType::Archiver).await;
        self.node_registry
            .send_to_all(payload.clone(), &NodeRegistryType::Executor).await;
        self.node_registry
            .send_to_all(payload, &NodeRegistryType::Sentinel).await;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use pneumatic_core::blocks::{Block, FinalityStatus};
    use pneumatic_core::config::Config;
    use pneumatic_core::crypto::AsymCryptoProvider;
    use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::messages::Message;
    use pneumatic_core::node::{NodeRegistryType, NodeType};
    use pneumatic_core::transactions::{SignedTransaction, TransactionCommit};
    use std::collections::HashMap;

    fn make_test_config() -> Config {
        let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
        let public_key = identity.ed25519.public_key().unwrap_or_default();
        let rhash = identity.rhash;
        Config {
            public_key,
            ip_address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Finalizer],
            main_environment_id: "test_env".to_string(),
            reconciliation_partition_id: "reconciliation".to_string(),
            environment_metadata: make_test_env_data(),
            // Capacity entries are required — without them get_max_node_number
            // returns 0 and register_peer rejects every peer.
            type_configs: Arc::new({
                let tc = DashMap::new();
                for t in [
                    NodeRegistryType::Committer,
                    NodeRegistryType::Sentinel,
                    NodeRegistryType::Executor,
                    NodeRegistryType::Finalizer,
                    NodeRegistryType::Archiver,
                ] {
                    tc.insert(t.clone(), pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 });
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

    fn make_test_env_data() -> Arc<DashMap<String, EnvironmentMetadata>> {
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
            let env = EnvironmentMetadata::load_from_spec(spec);
            env_map.insert(env.environment_id.clone(), env);
        }
        Arc::new(env_map)
    }

    fn make_test_node_registry() -> Arc<NodeRegistry> {
        let config = make_test_config();
        Arc::new(NodeRegistry::init(
            Arc::new(config),
            None,
            Arc::new(|_, _| true),
        ))
    }

    fn make_test_commit() -> TransactionCommit {
        TransactionCommit {
            trans_id: b"test_tx".to_vec(),
            token_id: vec![0, 1, 2],
            env_id: "test_env".to_string(),
            proposed_block: pneumatic_core::blocks::Block {
                signed_trans: SignedTransaction::test_transaction(),
                token_metadata: HashMap::new(),
                previous_hash: vec![1, 2, 3],
                current_hash: vec![4, 5, 6],
                timestamp: chrono::Utc::now().timestamp(),
                finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
                proposer_key: vec![],
                epoch_number: 0,
            },
        }
    }

    #[tokio::test]
    async fn test_send_to_committers() {
        let registry = make_test_node_registry();
        let dispatcher = MessageDispatcher::new(
            registry,
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
        );

        let commit = make_test_commit();
        let result = dispatcher.send_to_committers(commit).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_clear_to_sentinels() {
        let registry = make_test_node_registry();
        let dispatcher = MessageDispatcher::new(
            registry,
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
        );

        let result = dispatcher.send_clear_to_sentinels("test_tx_001").await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_block_finalized_serializes() {
        // Verify BlockFinalized serialization produces a valid MsgPack message
        let block = Block {
            signed_trans: SignedTransaction::test_transaction(),
            token_metadata: HashMap::new(),
            previous_hash: vec![1, 2, 3],
            current_hash: vec![4, 5, 6],
            timestamp: chrono::Utc::now().timestamp(),
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };

        let body = serialize_to_bytes_rmp(&block).expect("Block serialization should succeed");
        let message = Message {
            chain_id: "test_env".to_string(),
            action: String::from("BlockFinalized"),
            body,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };

        let payload = serialize_to_bytes_rmp(&message).expect("Message serialization should succeed");

        // Verify round-trip deserialization
        let recovered: Message = deserialize_rmp_to(&payload).expect("Round-trip should succeed");
        assert_eq!(recovered.action, "BlockFinalized");
        assert_eq!(recovered.chain_id, "test_env");

        let recovered_block: Block = deserialize_rmp_to(&recovered.body).expect("Block deserialization should succeed");
        assert_eq!(recovered_block.previous_hash, vec![1, 2, 3]);
        assert_eq!(recovered_block.current_hash, vec![4, 5, 6]);
    }

    // -----------------------------------------------------------------------
    // Phase 1.1 regression: every dispatch method signs with node identity
    // -----------------------------------------------------------------------

    /// A Connection that records each sent payload verbatim.
    struct RecordingConnection {
        recorder: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
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
    fn assert_signed_by(message: &Message, identity: &pneumatic_core::rns::identity::NodeIdentity) {
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

    /// Pull the first captured Message with the given action out of the recorder.
    fn captured_message(
        recorder: &std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        action: &str,
    ) -> Message {
        let raw = recorder
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .find(|raw| matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == action))
            .unwrap_or_else(|| panic!("no {} message captured", action));
        deserialize_rmp_to(&raw).expect("captured payload should be a Message")
    }

    fn make_dispatcher(
        registry: Arc<NodeRegistry>,
    ) -> (MessageDispatcher, Arc<pneumatic_core::rns::identity::NodeIdentity>) {
        let identity =
            Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory());
        (
            MessageDispatcher::new(registry, "test_env".to_string(), vec![1, 2, 3, 4], identity.clone()),
            identity,
        )
    }

    #[tokio::test]
    async fn commit_broadcast_signed_with_finalizer_identity() {
        let registry = make_test_node_registry();
        let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
        assert!(registry.register_peer(
            vec![0xA1; 32],
            [1u8; 16],
            &NodeRegistryType::Committer,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ));

        let (dispatcher, identity) = make_dispatcher(registry);
        dispatcher.send_to_committers(make_test_commit()).await.expect("send should succeed");

        let message = captured_message(&recorder, "Commit");
        assert_signed_by(&message, &identity);
    }

    #[tokio::test]
    async fn clear_broadcast_signed_with_finalizer_identity() {
        let registry = make_test_node_registry();
        let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
        assert!(registry.register_peer(
            vec![0xA2; 32],
            [2u8; 16],
            &NodeRegistryType::Sentinel,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ));

        let (dispatcher, identity) = make_dispatcher(registry);
        dispatcher.send_clear_to_sentinels("tx_1.1").await.expect("send should succeed");

        let message = captured_message(&recorder, "Clear");
        assert_signed_by(&message, &identity);
    }

    #[tokio::test]
    async fn block_finalized_broadcast_signed_with_finalizer_identity() {
        let registry = make_test_node_registry();
        let committer_recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
        let archiver_recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
        assert!(registry.register_peer(
            vec![0xA3; 32],
            [3u8; 16],
            &NodeRegistryType::Committer,
            Box::new(RecordingConnection { recorder: committer_recorder.clone() }),
        ));
        assert!(registry.register_peer(
            vec![0xA4; 32],
            [4u8; 16],
            &NodeRegistryType::Archiver,
            Box::new(RecordingConnection { recorder: archiver_recorder.clone() }),
        ));

        let (dispatcher, identity) = make_dispatcher(registry);
        let block = Block {
            signed_trans: SignedTransaction::test_transaction(),
            token_metadata: HashMap::new(),
            previous_hash: vec![1, 2, 3],
            current_hash: vec![4, 5, 6],
            timestamp: chrono::Utc::now().timestamp(),
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        dispatcher
            .send_block_finalized(block, Some(StakeSet::default()))
            .await
            .expect("send should succeed");

        let to_committer = captured_message(&committer_recorder, "BlockFinalized");
        assert_signed_by(&to_committer, &identity);
        let to_archiver = captured_message(&archiver_recorder, "BlockFinalized");
        assert_signed_by(&to_archiver, &identity);
    }

    #[tokio::test]
    async fn block_confirmed_vote_signed_with_finalizer_identity() {
        let registry = make_test_node_registry();
        let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
        assert!(registry.register_peer(
            vec![0xA5; 32],
            [5u8; 16],
            &NodeRegistryType::Executor,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ));

        let (dispatcher, identity) = make_dispatcher(registry);
        dispatcher.send_block_confirmed_vote(&[9, 9, 9]).await.expect("send should succeed");

        let message = captured_message(&recorder, "BlockConfirmed");
        assert_signed_by(&message, &identity);
    }

    #[tokio::test]
    async fn block_quorum_reached_signed_with_finalizer_identity() {
        let registry = make_test_node_registry();
        let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
        assert!(registry.register_peer(
            vec![0xA6; 32],
            [6u8; 16],
            &NodeRegistryType::Archiver,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ));

        let (dispatcher, identity) = make_dispatcher(registry);
        dispatcher.send_block_quorum_reached(&[7, 7, 7]).await.expect("send should succeed");

        let message = captured_message(&recorder, "BlockQuorumReached");
        assert_signed_by(&message, &identity);
    }
}
