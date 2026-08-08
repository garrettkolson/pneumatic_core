use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use pneumatic_core::data::DataProvider;
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
    /// Data provider for loading/saving user data (gas deduction)
    data_provider: Arc<dyn DataProvider>,
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
        data_provider: Arc<dyn DataProvider>,
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
    /// Flow:
    /// 1. Acquire lock on the transaction in the pending registry
    /// 2. Verify the transaction is in Finalizing state
    /// 3. Apply the block via BlockServices (commit + distribute)
    /// 4. Update transaction state to Committed
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

        // Step 2: Verify the transaction is in Finalizing state and extract
        // the inner transaction for later state transition
        let transaction = {
            let entry = self
                .pending_registry
                .get_transaction_mut(&tx_id)?;

            match &entry.state {
                TransactionState::Finalizing { transaction, .. } => transaction.clone(),
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

        // Step 4: Transition to Committed state
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
    use pneumatic_core::data::{DataError, DataProvider};
    use pneumatic_core::encoding::deserialize_rmp_to;
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::gossiper::Gossiper;
    use pneumatic_core::messages::Message;
    use pneumatic_core::node::registry::NodeRegistry;
    use pneumatic_core::node::NodeRegistryType;
    use pneumatic_core::registry::PendingTransactionRegistry;
    use pneumatic_core::transactions::{PendingTransaction, SignedTransaction, Transaction, TransactionCommit, TransactionSignature, TransactionState};
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
    }

    fn make_test_env_data() -> EnvironmentMetadata {
        let json = r#"{"environment_id":"test","environment_name":"test",
            "partitions":[{"id":"token","partition_type":"Token"},
            {"id":"slush","partition_type":"Slush"}],
            "asym_crypto_provider":{"RSA":null},"sym_crypto_provider":"sym",
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
        };

        let mut block = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
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
        let epoch_reconciler = Arc::new(EpochReconciler::new(
            data_provider_core.clone(),
            "test".to_string(),
        ));
        let hash_provider = Arc::new(BasicHashProvider::new());
        let leader_selector = Arc::new(LeaderSelector::new(hash_provider));

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
}
