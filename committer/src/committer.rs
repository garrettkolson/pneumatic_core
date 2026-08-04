use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use pneumatic_core::encoding::deserialize_rmp_to;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::epoch::{IEpochLeaderSelector, IEpochReconciler, IStakingManager};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::tokens::Token;
use pneumatic_core::transactions::{TransactionCommit, TransactionState};

use super::block_services::BlockServices;
use super::committer_error::CommitterError;
use super::epoch_manager::{EpochReconciler, LeaderSelector, StakeStore, StakingManager};

/// Convert a byte slice to a hex string (lowercase, no prefix).
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// ---------------------------------------------------------------------------
// Committer — receives TransactionCommit messages, commits blocks,
//             manages epoch transitions
// ---------------------------------------------------------------------------

/// The Committer is the terminal node in the pneumatic pipeline.
/// It receives `TransactionCommit` messages from the Finalizer,
/// validates and commits blocks to token blockchains, distributes
/// blocks to archivers, and manages epoch transitions.
///
/// Responsibilities:
/// 1. Receive and validate `TransactionCommit` messages (action: "Commit")
/// 2. Commit validated blocks to token blockchains
/// 3. Distribute committed blocks to archivers
/// 4. Handle token distribution from other committers
/// 5. Manage epoch transitions (staking, reconciliation, leader selection)
pub struct Committer {
    /// Environment metadata for validation and logging
    env_data: Arc<EnvironmentMetadata>,
    /// Public key of this committer node
    public_key: Vec<u8>,
    /// Gossiper for receiving messages
    gossiper: Arc<Gossiper>,
    /// Block services (token commit, block distribution, token distribution)
    block_services: Arc<BlockServices>,
    /// Shared registry of connected nodes
    node_registry: Arc<NodeRegistry>,
    /// Token cache — token_id -> Token, DashMap-backed for concurrency
    tokens: Arc<DashMap<Vec<u8>, Token>>,
    /// Transaction registry for state tracking
    pending_registry: Arc<PendingTransactionRegistry>,
    /// Stake store for epoch management
    stake_store: Arc<StakeStore>,
    /// Staking manager for epoch reconciliation
    staking_manager: Arc<StakingManager>,
    /// Epoch reconciler for chain analysis at epoch boundaries
    epoch_reconciler: Arc<EpochReconciler>,
    /// Leader selector for epoch transitions
    leader_selector: Arc<LeaderSelector>,
    /// Flag: is the committer shutting down?
    awaiting_shutdown: Arc<Mutex<bool>>,
}

impl Committer {
    /// Create a new Committer with all required components.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        env_data: Arc<EnvironmentMetadata>,
        public_key: Vec<u8>,
        gossiper: Arc<Gossiper>,
        block_services: Arc<BlockServices>,
        node_registry: Arc<NodeRegistry>,
        tokens: Arc<DashMap<Vec<u8>, Token>>,
        pending_registry: Arc<PendingTransactionRegistry>,
        stake_store: Arc<StakeStore>,
        staking_manager: Arc<StakingManager>,
        epoch_reconciler: Arc<EpochReconciler>,
        leader_selector: Arc<LeaderSelector>,
    ) -> Self {
        Committer {
            env_data,
            public_key,
            gossiper,
            block_services,
            node_registry,
            tokens,
            pending_registry,
            stake_store,
            staking_manager,
            epoch_reconciler,
            leader_selector,
            awaiting_shutdown: Arc::new(Mutex::new(false)),
        }
    }

    /// Initialize the committer — wire the gossiper's message handler.
    /// The closure receives deserialized messages and routes them
    /// to the appropriate `handle_*` method.
    pub fn initialize<F>(&self, on_message_received: F)
    where
        F: Fn(Message) + Send + Sync + 'static,
    {
        // Wrap the caller's Fn(Message) to deserialize from raw bytes.
        // The gossiper stores a Fn(Vec<u8>) and calls this wrapper
        // after deserialization and dedup checks pass.
        let wrapped = move |raw: Vec<u8>| {
            if let Ok(msg) = deserialize_rmp_to(&raw) {
                on_message_received(msg);
            }
            // Silently drop malformed messages — the gossiper already
            // recorded them in the dedup cache.
        };
        self.gossiper.initialize(wrapped);
    }

    /// Set the shutdown flag.
    pub async fn initiate_shutdown(&self) {
        *self.awaiting_shutdown.lock().await = true;
    }

    /// Check if the committer is shutting down.
    pub async fn is_shutting_down(&self) -> bool {
        *self.awaiting_shutdown.lock().await
    }

    /// Primary entry point — routes incoming messages by action.
    /// Called by the gossiper's message handler after deserialization.
    pub fn handle_message(&self, message: Message) -> Result<(), CommitterError> {
        match message.action.as_str() {
            "Commit" => self.handle_commit(message),
            "DistributeToken" => self.handle_token_distribution(message),
            "DistributeBlock" => self.handle_block_distribution(message),
            "EpochReconcile" => self.handle_epoch_reconcile(),
            action => Err(CommitterError::UnknownAction(action.to_string())),
        }
    }

    // -----------------------------------------------------------------------
    // Commit message handling
    // -----------------------------------------------------------------------

    /// Handle a "Commit" message — the core pipeline step.
    ///
    /// Flow:
    /// 1. Deserialize the TransactionCommit from the message body
    /// 2. Validate the transaction message (env_id, block hash)
    /// 3. Acquire lock and verify Finalizing state
    /// 4. Commit the block via BlockServices
    /// 5. Transition to Committed, release lock
    fn handle_commit(&self, message: Message) -> Result<(), CommitterError> {
        // Deserialize the TransactionCommit from the message body
        let commit: TransactionCommit =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        // Validate the transaction message
        self.validate_transaction_message(&commit)?;

        // Check and commit the transaction results
        self.check_and_commit_transaction_results(&commit)
    }

    /// Check and commit transaction results.
    ///
    /// Flow:
    /// 1. Acquire lock on the transaction in the pending registry
    /// 2. Verify the transaction is in Finalizing state
    /// 3. Apply the block via BlockServices (commit + distribute)
    /// 4. Update transaction state to Committed
    /// 5. Release the transaction lock
    fn check_and_commit_transaction_results(
        &self,
        commit: &TransactionCommit,
    ) -> Result<(), CommitterError> {
        let tx_id = String::from_utf8_lossy(&commit.trans_id).to_string();

        // Step 1: Acquire lock on the transaction
        self.pending_registry
            .acquire_transaction(&tx_id)
            .map_err(|_| CommitterError::TransactionNotInFinalizing(tx_id.clone()))?;

        // Step 2: Verify the transaction is in Finalizing state and extract
        // the inner transaction for later state transition
        let transaction = {
            let entry = self
                .pending_registry
                .get_transaction_mut(&tx_id)
                .ok_or_else(|| CommitterError::TransactionNotFound(tx_id.clone()))?;

            match &entry.state {
                TransactionState::Finalizing { transaction, .. } => transaction.clone(),
                _ => {
                    return Err(CommitterError::TransactionNotInFinalizing(tx_id));
                }
            }
        };

        // Step 3: Commit the block via BlockServices
        let result = self.block_services.commit_block(commit)?;

        // Step 4: Transition to Committed state
        if let Some(mut entry) = self.pending_registry.get_transaction_mut(&tx_id) {
            entry.transition_to_committed(transaction, result.token_id);
        }

        // Step 5: Release the transaction lock
        let should_remove = self.pending_registry.release_transaction(&tx_id)?;

        if should_remove {
            let _ = self.pending_registry.remove_transaction(&tx_id);
        }

        // Step 6: Distribute the committed block to archivers
        let _ = self.block_services.distribute_to_archivers(&commit.proposed_block);

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Token distribution handling
    // -----------------------------------------------------------------------

    /// Handle token distribution from other committers.
    /// Inserts the token into the local cache for future commits.
    fn handle_token_distribution(&self, message: Message) -> Result<(), CommitterError> {
        let token: Token = deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        let token_key = token.id.clone();
        self.tokens.insert(token_key, token);

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Block distribution handling
    // -----------------------------------------------------------------------

    /// Handle block distribution from other committers.
    /// Logs receipt for observability.
    fn handle_block_distribution(&self, message: Message) -> Result<(), CommitterError> {
        let block: pneumatic_core::blocks::Block =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        let logger = &self.env_data.logger;
        logger.log(format!(
            "Received distributed block (hash: {})",
            bytes_to_hex(&block.current_hash)
        ));

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Epoch reconciliation handling
    // -----------------------------------------------------------------------

    /// Handle epoch reconciliation request.
    ///
    /// Flow:
    /// 1. Run the epoch reconciler to analyze chain state
    /// 2. Apply staking operations from reconciliation
    /// 3. Select new leader for the epoch
    fn handle_epoch_reconcile(&self) -> Result<(), CommitterError> {
        // Run reconciliation
        let reconciliation = self.epoch_reconciler.reconcile();

        if !reconciliation.slashing_ops.is_empty() || !reconciliation.reward_ops.is_empty() {
            // Apply staking operations
            self.staking_manager.apply_ops(&reconciliation)?;
        }

        // Select new leader for the epoch
        let stake_set = self.stake_store.to_stake_set();
        let leader_key = self.leader_selector.select(&stake_set);

        let logger = &self.env_data.logger;
        if !leader_key.is_empty() {
            logger.log(format!(
                "Epoch leader selected: {}",
                bytes_to_hex(&leader_key)
            ));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    /// Validate a Commit message.
    ///
    /// Checks:
    /// 1. The commit's env_id matches this committer's environment
    /// 2. The proposed block's hash is non-empty
    fn validate_transaction_message(&self, commit: &TransactionCommit) -> Result<(), CommitterError> {
        // Environment ID check
        if commit.env_id != self.env_data.environment_id {
            return Err(CommitterError::EnvironmentMismatch {
                expected: self.env_data.environment_id.clone(),
                got: commit.env_id.clone(),
            });
        }

        // Block hash check
        if commit.proposed_block.current_hash.is_empty() {
            return Err(CommitterError::InvalidBlockHash);
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Utilities
    // -----------------------------------------------------------------------

    /// Add a token to the local cache (for bootstrapping).
    pub fn bootstrap_token(&self, token: Token) {
        self.tokens.insert(token.id.clone(), token);
    }

    /// Get the number of cached tokens.
    pub fn cached_token_count(&self) -> usize {
        self.tokens.len()
    }
}
