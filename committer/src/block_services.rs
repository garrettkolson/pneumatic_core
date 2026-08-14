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
}

impl BlockServices {
    pub fn new(
        tokens: Arc<DashMap<Vec<u8>, Token>>,
        data_provider: Arc<dyn DataProvider>,
        node_registry: Arc<NodeRegistry>,
        env_data: Arc<EnvironmentMetadata>,
        logger: Arc<dyn Logger>,
    ) -> Self {
        BlockServices {
            tokens,
            data_provider,
            node_registry,
            env_data,
            logger,
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

        let message = Message {
            chain_id: self.env_data.environment_id.clone(),
            action: String::from("DistributeBlock"),
            body: payload,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };

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

        let message = Message {
            chain_id: self.env_data.environment_id.clone(),
            action: String::from("DistributeToken"),
            body: payload,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };

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
