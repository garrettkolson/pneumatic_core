use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use pneumatic_core::data::DataProvider;
use pneumatic_core::encoding::deserialize_rmp_to;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::blocks::{Block, BlockFactory, Blockchain, FinalityStatus};
use pneumatic_core::epoch::{BlockProposer, EpochBoundaryDetector, IEpochLeaderSelector, IEpochReconciler, IBlockProposer, IStakingManager, StakeSet};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::logging::Logger;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::tokens::Token;
use pneumatic_core::transactions::{SignedTransaction, TransactionCommit, TransactionState};

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
    /// Data provider for loading/saving user data (gas deduction)
    data_provider: Arc<dyn DataProvider>,
    /// Current epoch number for deterministic leader selection
    current_epoch_number: AtomicU64,
    /// Flag: is the committer shutting down?
    awaiting_shutdown: Arc<Mutex<bool>>,
    /// Epoch boundary detector — leader checks expiry and advances epochs
    epoch_detector: Arc<Mutex<Option<EpochBoundaryDetector>>>,
    /// Block proposer — dequeues transactions from the pool for batch proposal
    block_proposer: Arc<dyn IBlockProposer>,
    /// Duration of each epoch in seconds
    epoch_duration: i64,
    /// Interval between proposal polls in milliseconds
    proposal_interval_ms: u64,
}

impl Committer {
    /// Create a new Committer with all required components.
    #[allow(clippy::too_many_arguments)]
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
        data_provider: Arc<dyn DataProvider>,
        current_epoch_number: u64,
        epoch_detector: Option<EpochBoundaryDetector>,
        block_proposer: Arc<dyn IBlockProposer>,
        epoch_duration: i64,
        proposal_interval_ms: u64,
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
            data_provider,
            current_epoch_number: AtomicU64::new(current_epoch_number),
            awaiting_shutdown: Arc::new(Mutex::new(false)),
            epoch_detector: Arc::new(Mutex::new(epoch_detector)),
            block_proposer,
            epoch_duration,
            proposal_interval_ms,
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
    pub async fn handle_message(&self, message: Message) -> Result<(), CommitterError> {
        match message.action.as_str() {
            "Commit" => self.handle_commit(message).await,
            "DistributeToken" => self.handle_token_distribution(message).await,
            "DistributeBlock" => self.handle_block_distribution(message).await,
            "EpochReconcile" => self.handle_epoch_reconcile().await,
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
    async fn handle_commit(&self, message: Message) -> Result<(), CommitterError> {
        // Deserialize the TransactionCommit from the message body
        let commit: TransactionCommit =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        // Validate the transaction message
        self.validate_transaction_message(&commit)?;

        // Check and commit the transaction results
        self.check_and_commit_transaction_results(&commit).await
    }

    /// Check and commit transaction results.
    ///
    /// Accepts transactions in two states:
    /// - **Finalizing**: standard pipeline (Sentinel → Executor → Finalizer → Committer).
    ///   The transaction was assigned a finalizer and collected quorum signatures.
    /// - **Validated**: leader-proposal path. The leader dequeued the transaction
    ///   from the pool and proposed it directly. No finalizer key is present.
    ///
    /// Flow:
    /// 1. Acquire lock on the transaction in the pending registry
    /// 2. Verify the transaction is in Finalizing OR Validated state
    /// 3. Apply the block via BlockServices (commit + distribute)
    /// 4. Update transaction state to Committed (or remove from pool for leader path)
    /// 5. Release the transaction lock
    async fn check_and_commit_transaction_results(
        &self,
        commit: &TransactionCommit,
    ) -> Result<(), CommitterError> {
        let tx_id = String::from_utf8_lossy(&commit.trans_id).to_string();

        // Step 1: Acquire lock on the transaction
        self.pending_registry
            .acquire_transaction(&tx_id)
            .map_err(|_| CommitterError::TransactionNotInFinalizing(tx_id.clone()))?;

        // Step 2: Extract transaction from either Finalizing (standard pipeline)
        // or Validated (leader-proposal) state.
        let (transaction, is_leader_proposal) = {
            let entry = self
                .pending_registry
                .get_transaction_mut(&tx_id)?;

            match &entry.state {
                TransactionState::Finalizing { transaction, .. } => {
                    (transaction.clone(), false)
                }
                TransactionState::Validated { transaction, .. } => {
                    (transaction.clone(), true)
                }
                _ => {
                    return Err(CommitterError::TransactionNotInFinalizing(tx_id));
                }
            }
        };

        // Step 3: Commit the block via BlockServices
        let result = self.block_services.commit_block(commit)?;

        // Step 3.5: Deduct gas from sender's fuel balance
        if let Some(gas_used) = self.pending_registry.get_gas_used(&tx_id) {
            if let Ok(mut user) = self.data_provider.get_user(
                &transaction.sender, &self.env_data.token_partition_id,
            ) {
                user.fuel_balance = user.fuel_balance.saturating_sub(gas_used);
                let _ = self.data_provider.save_user(
                    &transaction.sender, user, &self.env_data.token_partition_id,
                );
            }
        }

        // Step 4: Update transaction state
        if is_leader_proposal {
            // Leader-proposal path: remove from pool, then transition to Committed
            // so that release() returns true (entry cleanup requires Committed/Failed state).
            self.pending_registry.remove_from_pool(&tx_id);
        }
        // Transition to Committed for BOTH paths — release() checks for Committed/Failed
        // to decide whether to remove the entry when lock_count reaches 0.
        if let Ok(mut entry) = self.pending_registry.get_transaction_mut(&tx_id) {
            entry.transition_to_committed(transaction, result.token_id);
        }

        // Step 5: Release the transaction lock
        let should_remove = self.pending_registry.release_transaction(&tx_id)?;

        if should_remove {
            let _ = self.pending_registry.remove_transaction(&tx_id);
        }

        // Step 6: Distribute the committed block to archivers
        let _ = self.block_services.distribute_to_archivers(&commit.proposed_block).await;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Token distribution handling
    // -----------------------------------------------------------------------

    /// Handle token distribution from other committers.
    /// Inserts the token into the local cache for future commits.
    async fn handle_token_distribution(&self, message: Message) -> Result<(), CommitterError> {
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
    async fn handle_block_distribution(&self, message: Message) -> Result<(), CommitterError> {
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
    async fn handle_epoch_reconcile(&self) -> Result<(), CommitterError> {
        // Run reconciliation
        let reconciliation = self.epoch_reconciler.reconcile();

        if !reconciliation.slashing_ops.is_empty() || !reconciliation.reward_ops.is_empty() {
            // Apply staking operations
            self.staking_manager.apply_ops(&reconciliation)?;
        }

        // Select new leader for the epoch and persist stake snapshot
        let stake_set = self.stake_store.to_stake_set();
        let new_epoch_number = self.current_epoch_number.load(Ordering::SeqCst) + 1;
        let leader_key = self.leader_selector.select(&stake_set, new_epoch_number);
        self.current_epoch_number.store(new_epoch_number, Ordering::SeqCst);

        // Persist the frozen stake snapshot for this epoch (for sentinel deterministic routing)
        let _ = self.data_provider.save_stake_snapshot(
            new_epoch_number,
            stake_set,
            &self.env_data.token_partition_id,
        );

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

    /// Get the proposal poll interval in milliseconds.
    pub fn proposal_interval_ms(&self) -> u64 {
        self.proposal_interval_ms
    }

    /// Get a reference to the logger.
    pub fn logger(&self) -> &Arc<dyn Logger> {
        &self.env_data.logger
    }

    // -----------------------------------------------------------------------
    // Block proposal
    // -----------------------------------------------------------------------

    /// Advance to a new epoch: bump the epoch number, select a new leader,
    /// and save the previous leader for stale block detection.
    fn advance_epoch(&self) -> Option<Vec<u8>> {
        let mut detector = self.epoch_detector.try_lock().ok()?;
        let detector = detector.as_mut()?;
        let stake_set = self.stake_store.to_stake_set();
        detector.advance_to_new_epoch(
            self.leader_selector.as_ref(),
            &stake_set,
            self.epoch_duration,
        );
        let new_epoch_number = detector.current_epoch.epoch_number;
        self.current_epoch_number.store(new_epoch_number, Ordering::SeqCst);
        let new_leader = detector.current_epoch.leader_public_key.clone();

        // Persist the frozen stake snapshot for this epoch (for sentinel deterministic routing)
        let _ = self.data_provider.save_stake_snapshot(
            new_epoch_number,
            stake_set,
            &self.env_data.token_partition_id,
        );

        // Advance may have set previous_leader — that's fine, it stays in the detector.
        Some(new_leader)
    }

    /// Propose a batch of transactions for a given token.
    ///
    /// Steps:
    /// 1. Check epoch expiry — if expired, advance to new epoch
    /// 2. Verify this node is the current epoch leader
    /// 3. Dequeue transactions from the pool via BlockProposer
    /// 4. Build TransactionCommit for each dequeued transaction
    /// 5. Return commits for dispatch to the Finalizer
    pub async fn propose_blocks(
        &self,
        token_id: &[u8],
        limit: usize,
    ) -> Result<Vec<TransactionCommit>, CommitterError> {
        // Step 1: Check epoch expiry and advance if needed
        {
            let now = chrono::Utc::now().timestamp();
            let should_advance = {
                let detector = self.epoch_detector.lock().await;
                if let Some(ref d) = *detector {
                    d.is_epoch_expired(now)
                } else {
                    false
                }
            };

            if should_advance {
                if let Some(new_leader) = self.advance_epoch() {
                    let epoch_num = self.current_epoch_number.load(Ordering::SeqCst);
                    let logger = &self.env_data.logger;
                    logger.log(format!(
                        "Epoch advanced to {} (new leader: {})",
                        epoch_num,
                        bytes_to_hex(&new_leader),
                    ));
                }
            }
        }

        // Step 2: Check if this node is the current epoch leader
        let is_leader = {
            let detector = self.epoch_detector.lock().await;
            if let Some(ref d) = *detector {
                d.current_leader() == Some(self.public_key.as_slice())
            } else {
                false
            }
        };

        if !is_leader {
            return Ok(Vec::new());
        }

        // Step 3: Dequeue transactions from the pool
        let batch = self
            .block_proposer
            .propose_batch(&self.pending_registry, token_id, limit)
            .map_err(|e| CommitterError::Core(e))?;

        if batch.is_empty() {
            return Ok(Vec::new());
        }

        // Step 4: Build TransactionCommit for each dequeued transaction
        let env_id = self.env_data.environment_id.clone();
        let token_id_vec = token_id.to_vec();
        let mut commits = Vec::with_capacity(batch.len());

        for (tx, signed) in batch {
            let commit = TransactionCommit {
                trans_id: tx.id.into_bytes(),
                token_id: token_id_vec.clone(),
                env_id: env_id.clone(),
                proposed_block: Block::from_transaction(
                    signed,
                    Blockchain::new(),
                    &Token::new(),
                    0, // epoch_number: placeholder, set by Phase 5
                ),
            };
            commits.push(commit);
        }

        Ok(commits)
    }

    /// Run the epoch loop: iterate registered token IDs and propose blocks for each.
    pub async fn run_epoch_loop(&self) -> Result<(), CommitterError> {
        for token_id in self.tokens.iter().map(|r| r.key().clone()) {
            let _ = self.propose_blocks(&token_id, 10).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use dashmap::DashMap;

    use pneumatic_core::blocks::Block;
    use pneumatic_core::config::Config;
    use pneumatic_core::conns::factories::ConnFactory;
    use pneumatic_core::crypto::BasicHashProvider;
    use pneumatic_core::data::{DataError, DataProvider, StubDataProvider};
    use pneumatic_core::encoding::deserialize_rmp_to;
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::epoch::{BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector};
    use pneumatic_core::errors::TransactionRiskFactor;
    use pneumatic_core::gossiper::Gossiper;
    use pneumatic_core::messages::Message;
    use pneumatic_core::node::registry::NodeRegistry;
    use pneumatic_core::node::NodeRegistryType;
    use pneumatic_core::registry::PendingTransactionRegistry;
    use pneumatic_core::transactions::{PendingTransaction, SignedTransaction, Transaction, TransactionCommit, TransactionSignature, TransactionState, TransactionValidationResult};
    use pneumatic_core::user::User;

    use super::*;

    // --- In-memory DataProvider mock for tests ---

    struct TestDataProvider {
        users: Mutex<HashMap<Vec<u8>, HashMap<String, User>>>,
    }

    impl TestDataProvider {
        fn new() -> Self {
            Self {
                users: Mutex::new(HashMap::new()),
            }
        }
        fn insert_user(&self, key: Vec<u8>, partition_id: String, user: User) {
            self.users
                .lock()
                .unwrap()
                .entry(key)
                .or_default()
                .insert(partition_id, user);
        }
    }

    impl DataProvider for TestDataProvider {
        fn get_token(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Token, DataError> {
            Err(DataError::DataNotFound)
        }
        fn save_token(&self, _key: &Vec<u8>, _token: Token, _partition_id: &str) -> Result<(), DataError> {
            Ok(())
        }
        fn save_data(&self, _key: &Vec<u8>, _data: Vec<u8>, _partition_id: &str) -> Result<(), DataError> {
            Ok(())
        }
        fn get_user(&self, key: &Vec<u8>, partition_id: &str) -> Result<User, DataError> {
            self.users
                .lock()
                .unwrap()
                .get(key)
                .and_then(|partitions| partitions.get(partition_id))
                .cloned()
                .ok_or(DataError::DataNotFound)
        }
        fn save_user(&self, key: &Vec<u8>, user: User, partition_id: &str) -> Result<(), DataError> {
            self.users
                .lock()
                .unwrap()
                .entry(key.clone())
                .or_default()
                .insert(partition_id.to_string(), user);
            Ok(())
        }

        fn get_stake_snapshot(&self, _epoch: u64, _partition_id: &str) -> Result<StakeSet, DataError> {
            Ok(StakeSet::default())
        }

        fn save_stake_snapshot(&self, _epoch: u64, _snapshot: StakeSet, _partition_id: &str) -> Result<(), DataError> {
            Ok(())
        }
    }

    fn make_test_env_data() -> EnvironmentMetadata {
        let json = r#"{"environment_id":"test","environment_name":"test",
            "partitions":[{"id":"token","partition_type":"Token"},
            {"id":"slush","partition_type":"Slush"}],
            "asym_crypto_provider":{"Ed25519":null},"sym_crypto_provider":"sym",
            "serialization_provider":"rmp","quorum_percentage":67.0,
            "override_quorum_percentage":0.0,"max_risk":1.0,
            "allowed_token_types":[],"trans_validation_specs":[],
            "block_validation_specs":[],"log_file":"test.log"}"#;
        let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).unwrap();
        EnvironmentMetadata::load_from_spec(spec)
    }

    /// Create a block that chains off the token's current chain state.
    fn make_test_block_for_token(
        committer: &Committer,
        trans_id: &str,
    ) -> Block {
        // Get the chain's last hash (or genesis leader hash for empty chain)
        let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
            let token = entry.value();
            let state = token.blockchain.get_current_chain_state();
            if state.last_hash_in.is_empty() {
                // Empty chain — use a fixed leader hash
                vec![42u8; 32]
            } else {
                state.last_hash_in
            }
        } else {
            vec![42u8; 32]
        };

        let signed = SignedTransaction {
            transaction_id: trans_id.to_string(),
            transaction: Transaction {
                id: trans_id.to_string(),
                action: "Process".into(),
                token_id: vec![1],
                bid: None,
                sequence_number: 1,
                sender: b"alice".to_vec(),
                receiver: b"bob".to_vec(),
                amount: Some(100),
                timestamp: 0,
                result_hash: vec![],
            },
            total_voters: 3,
            total_stake: 42,
            leader_hash: prev_hash.clone(),
            leader_address: vec![],
            leader_stake: 0,
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
        };

        let mut block = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block);
        block
    }

    fn bootstrap_token_chain(committer: &Committer) {
        // Create a genesis block so validate_next_block has a previous_hash
        let prev_hash = vec![42u8; 32];
        let signed = SignedTransaction::test_transaction();
        let mut genesis = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        genesis.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&genesis);

        if let Some(mut entry) = committer.tokens.get_mut(&vec![1]) {
            entry.value_mut().blockchain.add_block(genesis);
        }
    }

    fn make_test_committer(
        data_provider: Arc<TestDataProvider>,
    ) -> (Committer, Arc<PendingTransactionRegistry>) {
        let env_data = Arc::new(make_test_env_data());
        let config = Config {
            public_key: vec![1],
            ip_address: "127.0.0.1".parse().unwrap(),
            rest_api_version: 1,
            node_type: pneumatic_core::node::NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            type_configs: Arc::new(DashMap::new()),
        };
        let conn_factory = ConnFactory::new();
        let on_received = Arc::new(|_data: Vec<u8>| {});
        let node_registry = Arc::new(NodeRegistry::init(
            Arc::new(config),
            Box::new(conn_factory),
            on_received,
        ));

        let gossiper = Arc::new(Gossiper::new(
            NodeRegistryType::Committer,
            Config {
                public_key: vec![2],
                ip_address: "127.0.0.1".parse().unwrap(),
                rest_api_version: 1,
                node_type: pneumatic_core::node::NodeType::Full,
                node_registry_types: vec![NodeRegistryType::Committer],
                main_environment_id: "test".to_string(),
                reconciliation_partition_id: "recon".to_string(),
                environment_metadata: Arc::new(DashMap::new()),
                type_configs: Arc::new(DashMap::new()),
            },
            60,
            env_data.asym_crypto_provider.clone(),
        ));

        let tokens = Arc::new(DashMap::new());
        let pending_registry = Arc::new(PendingTransactionRegistry::new());
        let stake_store = Arc::new(StakeStore::new());
        let staking_manager = Arc::new(StakingManager::new(stake_store.clone(), env_data.logger.clone()));
        let data_provider_core = Arc::new(pneumatic_core::data::DefaultDataProvider::new());
        let candidate_registry = Arc::new(CandidateRegistry::new());
        let epoch_reconciler = Arc::new(EpochReconciler::new(
            stake_store.clone(),
            candidate_registry,
            data_provider_core.clone(),
            "test".to_string(),
            vec![vec![1]], // token ID from bootstrap_token
        ));
        let hash_provider = Arc::new(BasicHashProvider::new());
        let leader_selector = Arc::new(LeaderSelector::new(hash_provider));

        // Epoch tracking components
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let epoch_duration = 300;
        let initial_epoch = Epoch::new_with_leader(
            1,
            now,
            now + epoch_duration,
            leader_selector.as_ref(),
            &stake_store.to_stake_set(),
        );
        let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
        let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));

        let block_services = Arc::new(BlockServices::new(
            tokens.clone(),
            data_provider_core.clone(),
            node_registry.clone(),
            env_data.clone(),
            env_data.logger.clone(),
        ));

        let committer = Committer::new(
            env_data.clone(),
            vec![1],
            gossiper,
            block_services,
            node_registry,
            tokens,
            pending_registry.clone(),
            stake_store,
            staking_manager,
            epoch_reconciler,
            leader_selector,
            data_provider,
            0,
            Some(epoch_detector),
            block_proposer,
            epoch_duration,
            5000,
        );

        (committer, pending_registry)
    }

    fn make_finalizing_entry(
        pending_registry: &PendingTransactionRegistry,
        tx_id: &str,
        sender: Vec<u8>,
    ) {
        pending_registry.register_pending(tx_id.to_string()).unwrap();
        let tx = Transaction {
            id: tx_id.to_string(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: sender.clone(),
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
        };
        // Transition directly via the internal map — register_pending creates Pending,
        // then we mutate to Finalizing state.
        {
            let mut entry = pending_registry.get_transaction_mut(tx_id).unwrap();
            entry.transition_to_validated(tx.clone(),
                pneumatic_core::transactions::TransactionValidationResult {
                    is_valid: true,
                    risk: pneumatic_core::errors::TransactionRiskFactor {
                        affected_parties: 2, amount: 100,
                        is_contract: false, is_multi_party: false,
                    },
                    failure_reasons: vec![],
                    finalizer_public_key: vec![3],
                });
            entry.transition_to_finalizing(tx, vec![3]);
        }
    }

    // --- Gas deduction tests ---

    #[tokio::test]
    async fn check_and_commit_deducts_gas_from_user() {
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
            public_key: b"alice".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry) = make_test_committer(dp.clone());

        // Bootstrap token and chain BEFORE creating block
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_gas_deduct";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
        registry.record_gas_used(tx_id, 50);

        let block = make_test_block_for_token(&committer, tx_id);
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit).await;
        if let Err(ref e) = result {
            eprintln!("Error: {:?}", e);
        }
        assert!(result.is_ok());

        let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 950);
    }

    #[tokio::test]
    async fn check_and_commit_no_gas_tracked_does_not_deduct() {
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"bob".to_vec(), "token".to_string(), User {
            public_key: b"bob".to_vec(),
            fuel_balance: 500,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry) = make_test_committer(dp.clone());

        // Bootstrap token and chain BEFORE creating block
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_no_gas";
        make_finalizing_entry(&registry, tx_id, b"bob".to_vec());
        // No record_gas_used called

        let block = make_test_block_for_token(&committer, tx_id);
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit).await;
        assert!(result.is_ok());

        let user = dp.get_user(&b"bob".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 500);
    }

    #[tokio::test]
    async fn check_and_commit_gas_exceeds_balance_saturates() {
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"charlie".to_vec(), "token".to_string(), User {
            public_key: b"charlie".to_vec(),
            fuel_balance: 100,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry) = make_test_committer(dp.clone());

        // Bootstrap token and chain BEFORE creating block
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_sat";
        make_finalizing_entry(&registry, tx_id, b"charlie".to_vec());
        registry.record_gas_used(tx_id, 200);

        let block = make_test_block_for_token(&committer, tx_id);
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit).await;
        assert!(result.is_ok());

        let user = dp.get_user(&b"charlie".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 0);
    }

    fn make_validated_entry(
        pending_registry: &PendingTransactionRegistry,
        tx_id: &str,
        sender: Vec<u8>,
    ) {
        pending_registry.register_pending(tx_id.to_string()).unwrap();
        let tx = Transaction {
            id: tx_id.to_string(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: sender.clone(),
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
        };
        // Transition to Validated (NOT Finalizing) — simulates leader-proposal path
        {
            let mut entry = pending_registry.get_transaction_mut(tx_id).unwrap();
            entry.transition_to_validated(tx.clone(),
                pneumatic_core::transactions::TransactionValidationResult {
                    is_valid: true,
                    risk: pneumatic_core::errors::TransactionRiskFactor {
                        affected_parties: 2, amount: 100,
                        is_contract: false, is_multi_party: false,
                    },
                    failure_reasons: vec![],
                    finalizer_public_key: vec![3],
                });
        }
    }

    #[tokio::test]
    async fn check_and_commit_validated_state_succeeds() {
        // Leader-proposal path: transaction is in Validated state (not Finalizing).
        // The Committer should still commit the block and transition to Committed.
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
            public_key: b"alice".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry) = make_test_committer(dp.clone());

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_leader_proposal";
        make_validated_entry(&registry, tx_id, b"alice".to_vec());
        registry.record_gas_used(tx_id, 75);

        let block = make_test_block_for_token(&committer, tx_id);
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit).await;
        assert!(result.is_ok());

        // Gas was deducted
        let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 925);

        // Transaction was removed from pool (leader-proposal path)
        assert!(!registry.contains(tx_id));
    }

    #[tokio::test]
    async fn check_and_commit_validated_saturates_on_overflow() {
        // Leader-proposal path with gas exceeding balance — should saturate to 0
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"bob".to_vec(), "token".to_string(), User {
            public_key: b"bob".to_vec(),
            fuel_balance: 50,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry) = make_test_committer(dp.clone());

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_leader_overflow";
        make_validated_entry(&registry, tx_id, b"bob".to_vec());
        registry.record_gas_used(tx_id, 200); // exceeds balance

        let block = make_test_block_for_token(&committer, tx_id);
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit).await;
        assert!(result.is_ok());

        let user = dp.get_user(&b"bob".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 0);
    }

    // --- propose_blocks and advance_epoch tests ---

    /// Build a Committer where the leader is controlled by `leader_key`.
    /// The committer's own public key is `committer_key`.
    fn make_committer_for_leader_test(
        committer_key: Vec<u8>,
        leader_key: Vec<u8>,
    ) -> (Committer, Arc<PendingTransactionRegistry>, Arc<TestDataProvider>) {
        let env_data = Arc::new(make_test_env_data());
        let config = Config {
            public_key: committer_key.clone(),
            ip_address: "127.0.0.1".parse().unwrap(),
            rest_api_version: 1,
            node_type: pneumatic_core::node::NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            type_configs: Arc::new(DashMap::new()),
        };
        let conn_factory = ConnFactory::new();
        let on_received = Arc::new(|_data: Vec<u8>| {});
        let node_registry = Arc::new(NodeRegistry::init(
            Arc::new(config),
            Box::new(conn_factory),
            on_received,
        ));

        let gossiper = Arc::new(Gossiper::new(
            NodeRegistryType::Committer,
            Config {
                public_key: vec![2],
                ip_address: "127.0.0.1".parse().unwrap(),
                rest_api_version: 1,
                node_type: pneumatic_core::node::NodeType::Full,
                node_registry_types: vec![NodeRegistryType::Committer],
                main_environment_id: "test".to_string(),
                reconciliation_partition_id: "recon".to_string(),
                environment_metadata: Arc::new(DashMap::new()),
                type_configs: Arc::new(DashMap::new()),
            },
            60,
            env_data.asym_crypto_provider.clone(),
        ));

        let tokens = Arc::new(DashMap::new());
        let pending_registry = Arc::new(PendingTransactionRegistry::new());
        let stake_store = Arc::new(StakeStore::new());
        let staking_manager = Arc::new(StakingManager::new(stake_store.clone(), env_data.logger.clone()));
        let data_provider_core = Arc::new(pneumatic_core::data::DefaultDataProvider::new());
        let candidate_registry = Arc::new(CandidateRegistry::new());
        let epoch_reconciler = Arc::new(EpochReconciler::new(
            stake_store.clone(),
            candidate_registry,
            data_provider_core.clone(),
            "test".to_string(),
            vec![vec![1]],
        ));
        let hash_provider = Arc::new(BasicHashProvider::new());
        let leader_selector = Arc::new(LeaderSelector::new(hash_provider));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let epoch_duration = 3600; // 1 hour — is_epoch_expired returns false
        let initial_epoch = Epoch {
            epoch_number: 1,
            start_timestamp: now,
            end_timestamp: now + epoch_duration,
            leader_public_key: leader_key.clone(),
        };
        let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
        let block_proposer = Arc::new(BlockProposer::new(leader_key, 100, vec![]));

        let block_services = Arc::new(BlockServices::new(
            tokens.clone(),
            data_provider_core.clone(),
            node_registry.clone(),
            env_data.clone(),
            env_data.logger.clone(),
        ));

        let test_dp = Arc::new(TestDataProvider::new());
        let committer = Committer::new(
            env_data,
            committer_key,
            gossiper,
            block_services,
            node_registry,
            tokens,
            pending_registry.clone(),
            stake_store,
            staking_manager,
            epoch_reconciler,
            leader_selector,
            test_dp.clone(),
            1,
            Some(epoch_detector),
            block_proposer,
            epoch_duration,
            5000,
        );

        (committer, pending_registry, test_dp)
    }

    #[tokio::test]
    async fn propose_blocks_returns_empty_when_not_leader() {
        let (committer, _registry, _dp) = make_committer_for_leader_test(
            vec![99], // committer key
            b"leader".to_vec(), // leader key — different
        );

        let result = committer.propose_blocks(&[1], 10).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn propose_blocks_returns_batch_when_leader_with_pool_items() {
        let (committer, registry, _dp) = make_committer_for_leader_test(
            b"leader".to_vec(), // committer key
            b"leader".to_vec(), // leader key — same, so this IS the leader
        );

        // Bootstrap token so it's in the cache
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);

        // Add a transaction to the pool
        let tx_id = "tx_propose_1".to_string();
        registry.register_pending(tx_id.clone()).unwrap();
        let tx = Transaction {
            id: tx_id.clone(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b"sender".to_vec(),
            receiver: b"receiver".to_vec(),
            amount: Some(50),
            timestamp: 5000,
            result_hash: vec![],
        };
        registry.transition_to_validated_and_enqueue(
            &tx_id,
            tx.clone(),
            TransactionValidationResult {
                is_valid: true,
                risk: TransactionRiskFactor {
                    affected_parties: 2,
                    amount: 50,
                    is_contract: false,
                    is_multi_party: false,
                },
                failure_reasons: vec![],
                finalizer_public_key: vec![5],
            },
        ).unwrap();

        // This committer IS the leader and has pool items — should return a batch
        let result = committer.propose_blocks(&[1], 10).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].trans_id, tx_id.into_bytes());
    }

    #[tokio::test]
    async fn advance_epoch_bumps_number() {
        let (committer, _registry, _dp) = make_committer_for_leader_test(
            b"leader".to_vec(),
            b"leader".to_vec(),
        );

        // Add a staker so leader selection produces a non-empty result
        committer.stake_store.add_staker(b"leader".to_vec(), 100);

        let before = committer.current_epoch_number.load(Ordering::SeqCst);
        let new_leader = committer.advance_epoch();

        let after = committer.current_epoch_number.load(Ordering::SeqCst);
        assert_eq!(after, before + 1);
        assert!(!new_leader.unwrap().is_empty());
    }
}
