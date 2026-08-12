use std::sync::Arc;

use pneumatic_core::encoding::serialize_to_bytes_rmp;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
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
    /// Signature from this finalizer (for outgoing messages)
    finalizer_signature: Vec<u8>,
}

impl MessageDispatcher {
    /// Create a new MessageDispatcher.
    pub fn new(
        node_registry: Arc<NodeRegistry>,
        env_id: String,
        public_key: Vec<u8>,
        finalizer_signature: Vec<u8>,
    ) -> Self {
        MessageDispatcher {
            node_registry,
            env_id,
            public_key,
            finalizer_signature,
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

        // Package as a wire message
        let message = Message {
            chain_id: self.env_id.clone(),
            action: String::from("Commit"),
            body: msg_body,
            signature: self.finalizer_signature.clone(),
            public_key: self.public_key.clone(),
        };

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

        // Package as a wire message
        let message = Message {
            chain_id: self.env_id.clone(),
            action: String::from("Clear"),
            body: msg_body,
            signature: self.finalizer_signature.clone(),
            public_key: self.public_key.clone(),
        };

        let payload = serialize_to_bytes_rmp(&message)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Broadcast to all sentinels via registered connections
        self.node_registry.send_to_all(payload, &NodeRegistryType::Sentinel).await;

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
    use pneumatic_core::config::Config;
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::node::{NodeRegistryType, NodeType};
    use pneumatic_core::transactions::{SignedTransaction, TransactionCommit};
    use std::collections::HashMap;

    fn make_test_config() -> Config {
        Config {
            public_key: vec![1, 2, 3, 4],
            ip_address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Finalizer],
            main_environment_id: "test_env".to_string(),
            reconciliation_partition_id: "reconciliation".to_string(),
            environment_metadata: make_test_env_data(),
            type_configs: Arc::new(DashMap::new()),
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
        Arc::new(
            NodeRegistry::init(
                Arc::new(config),
                Box::new(pneumatic_core::conns::factories::ConnFactory::new()),
                Arc::new(|_| {}),
            )
        )
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
            vec![5, 6, 7, 8],
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
            vec![5, 6, 7, 8],
        );

        let result = dispatcher.send_clear_to_sentinels("test_tx_001").await;
        assert!(result.is_ok());
    }
}
