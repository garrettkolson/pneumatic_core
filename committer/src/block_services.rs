use std::sync::Arc;

use dashmap::DashMap;

use pneumatic_core::blocks::Block;
use pneumatic_core::data::DataProvider;
use pneumatic_core::encoding::serialize_to_bytes_rmp;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::logging::Logger;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::tokens::{Token, TokenCommitResult};
use pneumatic_core::transactions::TransactionCommit;

use super::committer_error::CommitterError;

/// Convert bytes to lowercase hex string.
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Handles token commits, block distribution, and token distribution.
/// Holds a local cache of tokens for fast commit access.
pub struct BlockServices {
    /// Local token cache — token_id -> Token
    tokens: Arc<DashMap<Vec<u8>, Token>>,
    /// Data provider for loading/saving tokens from external storage
    data_provider: Arc<dyn DataProvider>,
    /// Node registry for broadcasting to other nodes
    node_registry: Arc<NodeRegistry>,
    /// Environment metadata for block validation
    env_data: Arc<EnvironmentMetadata>,
    /// Logger for commit events
    logger: Arc<dyn Logger>,
    /// Node identity — signs all outgoing distribution messages
    identity: Arc<NodeIdentity>,
}

impl BlockServices {
    pub fn new(
        tokens: Arc<DashMap<Vec<u8>, Token>>,
        data_provider: Arc<dyn DataProvider>,
        node_registry: Arc<NodeRegistry>,
        env_data: Arc<EnvironmentMetadata>,
        logger: Arc<dyn Logger>,
        identity: Arc<NodeIdentity>,
    ) -> Self {
        BlockServices {
            tokens,
            data_provider,
            node_registry,
            env_data,
            logger,
            identity,
        }
    }

    /// Commit a transaction by applying the proposed block to the token's blockchain.
    ///
    /// Flow:
    /// 1. Get the token from local cache
    /// 2. Call Token::commit_block (handles validation + chain append)
    /// 3. Update local cache with the modified token
    /// 4. Return the commit result
    pub fn commit_block(
        &self,
        commit: &TransactionCommit,
        // AUDIT Phase 5.2 / H2: when set and matching the current tip, roll the
        // conflicting tip back before appending the winner of a resolved conflict.
        rollback_tip_hash: Option<Vec<u8>>,
    ) -> Result<TokenCommitResult, CommitterError> {
        // Verify environment ID match
        if commit.env_id != self.env_data.environment_id {
            return Err(CommitterError::EnvironmentMismatch {
                expected: self.env_data.environment_id.clone(),
                got: commit.env_id.clone(),
            });
        }

        let token_key = commit.token_id.clone();

        let mut token_entry = self.tokens.get_mut(&token_key).ok_or_else(|| {
            CommitterError::TokenNotFound(bytes_to_hex(&token_key))
        })?;

        // Token::commit_block validates the block, trims if needed,
        // computes hash, appends to chain, increments sequence.
        // Takes ownership of the block (computes and sets current_hash).
        let result = token_entry.value_mut().commit_block(
            commit.proposed_block.clone(),
            false, // not an archiver
            &self.env_data,
            rollback_tip_hash.as_deref(),
        )?;

        self.logger.log(format!(
            "Committed block to token [{}] (chain length: {}, seq: {})",
            bytes_to_hex(&result.token_id),
            result.new_chain_length,
            result.sequence_number,
        ));

        Ok(result)
    }

    /// Distribute a committed block to all archiver nodes.
    ///
    /// Serializes the block and broadcasts to Archiver node type.
    /// Note: NodeRegistry.get_nodes(Archiver) currently returns None,
    /// so this will log a warning until the registry is updated.
    pub async fn distribute_to_archivers(&self, block: &Block) -> Result<(), CommitterError> {
        let payload = serialize_to_bytes_rmp(block)?;

        let message = Message::signed(
            self.env_data.environment_id.clone(),
            "DistributeBlock",
            payload,
            None,
            &self.identity,
        )?;

        let message_payload = serialize_to_bytes_rmp(&message)?;

        // NodeRegistry.send_to_all handles the broadcast via registered connections
        // Note: Archiver nodes are not yet supported in get_nodes()
        self.node_registry.send_to_all(message_payload, &NodeRegistryType::Archiver).await;

        self.logger.log(format!(
            "Attempted to distribute block to archivers (hash: {})",
            bytes_to_hex(&block.current_hash)
        ));

        Ok(())
    }

    /// Distribute a token to other committers (for token initialization
    /// on a new node joining the network).
    pub async fn distribute_token(&self, token_id: &[u8]) -> Result<(), CommitterError> {
        let token = self.tokens.get(token_id).ok_or_else(|| {
            CommitterError::TokenNotFound(bytes_to_hex(token_id))
        })?;

        let token_clone = token.value().clone();
        drop(token);

        let payload = serialize_to_bytes_rmp(&token_clone)?;

        let message = Message::signed(
            self.env_data.environment_id.clone(),
            "DistributeToken",
            payload,
            None,
            &self.identity,
        )?;

        let message_payload = serialize_to_bytes_rmp(&message)?;
        self.node_registry
            .send_to_all(message_payload, &NodeRegistryType::Committer).await;

        self.logger.log(format!(
            "Distributed token [{}]",
            bytes_to_hex(token_id)
        ));

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pneumatic_core::blocks::FinalityStatus;
    use pneumatic_core::config::Config;
    use pneumatic_core::crypto::{AsymCryptoProvider, Ed25519Provider};
    use pneumatic_core::data::DefaultDataProvider;
    use pneumatic_core::encoding::deserialize_rmp_to;
    use pneumatic_core::environment::EnvironmentMetadataSpec;
    use pneumatic_core::logging::FileLogger;
    use pneumatic_core::node::{NodeRegistryType, NodeType, NodeTypeConfig};
    use pneumatic_core::rns::identity::NodeIdentity;
    use pneumatic_core::transactions::{SignedTransaction, Transaction, TransactionSignature};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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

    fn make_test_env_data() -> Arc<EnvironmentMetadata> {
        let json = r#"{"environment_id":"test","environment_name":"test",
            "partitions":[{"id":"token","partition_type":"Token"},
            {"id":"slush","partition_type":"Slush"}],
            "asym_crypto_provider":{"Ed25519":null},"sym_crypto_provider":"sym",
            "serialization_provider":"rmp","quorum_percentage":67.0,
            "override_quorum_percentage":0.0,"max_risk":1.0,
            "allowed_token_types":[],"trans_validation_specs":[],
            "block_validation_specs":[],"log_file":"test.log"}"#;
        let spec: EnvironmentMetadataSpec = serde_json::from_str(json).expect("spec JSON");
        Arc::new(EnvironmentMetadata::load_from_spec(spec))
    }

    fn make_registry() -> (Arc<NodeRegistry>, Arc<NodeIdentity>) {
        let identity = Arc::new(NodeIdentity::generate_in_memory());
        let rhash = identity.rhash;
        let type_configs = DashMap::new();
        type_configs.insert(
            NodeRegistryType::Archiver.clone(),
            NodeTypeConfig { min: 1, max: 10, min_stake: 0 },
        );
        type_configs.insert(
            NodeRegistryType::Committer.clone(),
            NodeTypeConfig { min: 1, max: 10, min_stake: 0 },
        );
        let config = Config {
            public_key: identity.ed25519.public_key().unwrap_or_default(),
            ip_address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            type_configs: Arc::new(type_configs),
            identity: identity.clone(),
            rhash,
            bootstrap_peers: Vec::new(),
            rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
            transport_enabled: false,
        };
        (
            Arc::new(NodeRegistry::init(
                Arc::new(config),
                None,
                Arc::new(|_, _| true),
            )),
            identity,
        )
    }

    fn make_block() -> Block {
        Block {
            signed_trans: SignedTransaction {
                transaction_id: "dist_tx".to_string(),
                transaction: Transaction {
                    id: "dist_tx".to_string(),
                    action: "Process".into(),
                    token_id: vec![1],
                    bid: None,
                    sequence_number: 1,
                    sender: b"alice".to_vec(),
                    receiver: b"bob".to_vec(),
                    amount: Some(100),
                    timestamp: 0,
                    result_hash: vec![],
                    sender_signature: vec![],
                },
                total_voters: 3,
                total_stake: 42,
                leader_address: vec![],
                leader_stake: 0,
                leader_hash: vec![1, 2, 3],
                finalizer_addr: vec![],
                finalizer_sig: TransactionSignature {
                    transaction_id: vec![],
                    env_id: vec![],
                    transaction_hash: vec![],
                    signature: vec![],
                    current_stake: 0,
                },
                executor_sigs: HashMap::new(),
                proposer_key: vec![],
            },
            token_metadata: HashMap::new(),
            previous_hash: vec![1, 2, 3],
            current_hash: vec![4, 5, 6],
            timestamp: 0,
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        }
    }

    #[tokio::test]
    async fn distribute_to_archivers_signed_with_committer_identity() {
        let (registry, identity) = make_registry();
        let recorder = Arc::new(Mutex::new(Vec::new()));
        assert!(registry.register_peer(
            vec![0xAA; 32],
            [1u8; 16],
            &NodeRegistryType::Archiver,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ));

        let services = BlockServices::new(
            Arc::new(DashMap::new()),
            Arc::new(DefaultDataProvider::new()),
            registry,
            make_test_env_data(),
            Arc::new(FileLogger::new("/tmp/test_block_services.log".to_string())),
            identity.clone(),
        );

        services.distribute_to_archivers(&make_block()).await.expect("distribution should succeed");

        assert_eq!(recorder.lock().unwrap().len(), 1, "archiver should receive exactly one message");
        let raw = recorder.lock().unwrap()[0].clone();
        let message: Message = deserialize_rmp_to(&raw).expect("captured payload should be a Message");
        assert_eq!(message.action, "DistributeBlock");
        assert_signed_by(&message, &identity);
    }

    #[tokio::test]
    async fn distribute_token_signed_with_committer_identity() {
        let (registry, identity) = make_registry();
        let recorder = Arc::new(Mutex::new(Vec::new()));
        assert!(registry.register_peer(
            vec![0xBB; 32],
            [2u8; 16],
            &NodeRegistryType::Committer,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ));

        let tokens = Arc::new(DashMap::new());
        let mut token = Token::new();
        token.id = vec![1];
        tokens.insert(token.id.clone(), token);

        let services = BlockServices::new(
            tokens,
            Arc::new(DefaultDataProvider::new()),
            registry,
            make_test_env_data(),
            Arc::new(FileLogger::new("/tmp/test_block_services.log".to_string())),
            identity.clone(),
        );

        services.distribute_token(&[1]).await.expect("distribution should succeed");

        assert_eq!(recorder.lock().unwrap().len(), 1, "committer peer should receive exactly one message");
        let raw = recorder.lock().unwrap()[0].clone();
        let message: Message = deserialize_rmp_to(&raw).expect("captured payload should be a Message");
        assert_eq!(message.action, "DistributeToken");
        assert_signed_by(&message, &identity);
    }
}
