use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use pneumatic_core::crypto::HashProvider;
use pneumatic_core::data::{DataError, DataProvider};
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::errors::ValidationFailureReason;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::transactions::Transaction;

// ---------------------------------------------------------------------------
// Executor — transaction computation node
// ---------------------------------------------------------------------------

/// Executor receives preloaded transactions, fetches contract/user/token data,
/// executes the contract logic, validates results, and sends execution outputs
/// to the assigned Finalizer for signature collection.
///
/// Key design:
/// - **Backpressure**: configurable `max_in_flight` limits concurrent executions.
///   If the limit is reached, new transactions are rejected immediately.
/// - **Preload**: fetches all necessary data (contract, user, token, proxy auths)
///   from the DataProvider before execution.
/// - **Result**: hashes execution output and sends it to the Finalizer.
pub struct Executor {
    /// Environment ID for this executor
    env_id: String,
    /// Public key of this executor node
    public_key: Vec<u8>,
    /// Shared registry of connected nodes
    node_registry: Arc<NodeRegistry>,
    /// Data provider for fetching contract/user/token data
    data_provider: Arc<dyn DataProvider>,
    /// Transaction registry for state tracking
    pending_registry: Arc<PendingTransactionRegistry>,
    /// Hash provider for result hashing
    hash_provider: Arc<dyn HashProvider>,
    /// Backpressure: in-flight execution tasks keyed by transaction ID
    preload_tasks: Arc<Mutex<HashMap<String, Arc<DashMap<String, ExecutionResult>>>>>,
    /// Maximum number of concurrent execution tasks before backpressure kicks in
    max_in_flight: usize,
}

impl Executor {
    /// Create a new Executor with all required dependencies.
    ///
    /// `max_in_flight` controls backpressure — when this many tasks are running,
    /// new transactions will be rejected until a slot opens up.
    pub fn new(
        env_id: String,
        public_key: Vec<u8>,
        node_registry: Arc<NodeRegistry>,
        data_provider: Arc<dyn DataProvider>,
        pending_registry: Arc<PendingTransactionRegistry>,
        hash_provider: Arc<dyn HashProvider>,
        max_in_flight: usize,
    ) -> Self {
        Executor {
            env_id,
            public_key,
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            preload_tasks: Arc::new(Mutex::new(HashMap::new())),
            max_in_flight,
        }
    }

    /// Check if the executor is at capacity.
    /// Returns `true` if rejecting new transactions (backpressure).
    pub async fn is_at_capacity(&self) -> bool {
        let tasks = self.preload_tasks.lock().await;
        tasks.len() >= self.max_in_flight
    }

    /// Get the number of currently in-flight execution tasks.
    pub async fn in_flight_count(&self) -> usize {
        let tasks = self.preload_tasks.lock().await;
        tasks.len()
    }

    /// Preload data for a transaction and begin execution.
    ///
    /// Returns `Err(ExecutorError::AtCapacity)` if backpressure has kicked in.
    /// Returns `Err(ExecutorError::Registry)` if the transaction isn't found
    /// or is in a terminal state.
    ///
    /// Flow:
    /// 1. Acquire lock on the transaction in the pending registry
    /// 2. Check backpressure — reject if at capacity
    /// 3. Spawn an async task that executes the transaction
    /// 4. Track the task in `preload_tasks` for backpressure management
    pub async fn preload_for_transaction(&self, tx_id: &str) -> Result<(), ExecutorError> {
        // Step 1: Acquire lock on the transaction
        if self.pending_registry.acquire_transaction(tx_id).is_err() {
            return Err(ExecutorError::Registry(format!(
                "Transaction {} not found or in terminal state", tx_id
            )));
        }

        // Step 2: Check backpressure
        let at_capacity = self.is_at_capacity().await;
        if at_capacity {
            return Err(ExecutorError::AtCapacity {
                max_in_flight: self.max_in_flight,
                current: self.preload_tasks.lock().await.len(),
            });
        }

        // Step 3: Spawn the execution task
        let task_results = Arc::new(DashMap::new());
        let results_handle = task_results.clone();

        let handle = self.clone_handle();
        handle.execute_task(tx_id.to_string(), task_results).await;

        // Step 4: Track the task for backpressure
        self.preload_tasks.lock().await.insert(tx_id.to_string(), results_handle);

        Ok(())
    }

    /// Check if a preload task has completed and collect its results.
    pub async fn preload_cleanup(&self, tx_id: &str) {
        // Remove the task from tracking (backpressure slot freed)
        self.preload_tasks.lock().await.remove(tx_id);
    }

    /// Send execution result to the Finalizer for signature collection.
    ///
    /// Packages the execution result as a message and broadcasts to all
    /// Finalizer nodes in the target environment.
    async fn send_to_finalizer(
        &self,
        tx_id: &str,
        execution_result: Vec<u8>,
        result_hash: Vec<u8>,
        finalizer_key: &[u8],
    ) -> Result<(), ExecutorError> {
        // Look up the finalizer nodes in the registry
        let finalizer_nodes = self
            .node_registry
            .get_nodes(&NodeRegistryType::Finalizer)
            .ok_or_else(|| ExecutorError::NoFinalizers("No finalizers registered".to_string()))?;

        if finalizer_nodes.is_empty() {
            return Err(ExecutorError::NoFinalizers(
                "No finalizers registered".to_string(),
            ));
        }

        // Package execution result as a message
        let msg_body = serialize_to_bytes_rmp(&ExecutionResult {
            transaction_id: tx_id.to_string(),
            result_data: execution_result,
            result_hash,
        }).map_err(|e| ExecutorError::Encoding(e))?;

        let message = Message {
            chain_id: self.env_id.clone(),
            action: String::from("Execute"),
            body: msg_body,
            signature: finalizer_key.to_vec(),
            public_key: self.public_key.clone(),
        };

        let payload = serialize_to_bytes_rmp(&message).map_err(|e| ExecutorError::Encoding(e))?;

        // Broadcast to all finalizers (stub — actual networking via registered connections)
        self.node_registry.send_to_all(payload, &NodeRegistryType::Finalizer);

        Ok(())
    }

    /// Validate execution results against expected constraints.
    fn validate_execution_result(
        &self,
        _tx: &Transaction,
        result: &ExecutionResult,
    ) -> Result<(), Vec<ValidationFailureReason>> {
        let mut reasons = vec![];

        if result.result_hash.is_empty() {
            reasons.push(ValidationFailureReason::ContractNotFound);
        }

        if !reasons.is_empty() {
            return Err(reasons);
        }

        Ok(())
    }

    /// Get the finalizer public key from the transaction's validation result.
    fn get_finalizer_key(&self, tx_id: &str) -> Vec<u8> {
        if let Some(entry) = self.pending_registry.get_transaction_mut(tx_id) {
            if let pneumatic_core::transactions::TransactionState::Validated { validation, .. } =
                &entry.state
            {
                return validation.finalizer_public_key.clone();
            }
        }
        vec![]
    }

    fn clone_handle(&self) -> ExecutorHandle {
        ExecutorHandle {
            env_id: self.env_id.clone(),
            public_key: self.public_key.clone(),
            node_registry: self.node_registry.clone(),
            data_provider: self.data_provider.clone(),
            pending_registry: self.pending_registry.clone(),
            hash_provider: self.hash_provider.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// ExecutorHandle — lightweight clone for use in spawned tasks
// ---------------------------------------------------------------------------

/// A lightweight handle to the Executor, designed for use in async tasks.
/// Stores individual config values instead of the full Config struct to avoid
/// the need for Clone on Config.
#[derive(Clone)]
struct ExecutorHandle {
    env_id: String,
    public_key: Vec<u8>,
    node_registry: Arc<NodeRegistry>,
    data_provider: Arc<dyn DataProvider>,
    pending_registry: Arc<PendingTransactionRegistry>,
    hash_provider: Arc<dyn HashProvider>,
}

impl ExecutorHandle {
    /// Spawn an async execution task for a transaction.
    async fn execute_task(
        mut self,
        tx_id: String,
        results: Arc<DashMap<String, ExecutionResult>>,
    ) {
        tokio::spawn(async move {
            let result = self.run_execution(&tx_id).await;
            if let Ok(exec_result) = result {
                results.insert(tx_id, exec_result);
            }
        });
    }

    /// Run the full execution pipeline for a single transaction.
    async fn run_execution(&self, tx_id: &str) -> Result<ExecutionResult, ExecutorError> {
        // Step 1: Load the pending transaction from the registry
        let transaction = match self.pending_registry.get_transaction_mut(tx_id) {
            Some(mut entry) => {
                match &entry.state {
                    pneumatic_core::transactions::TransactionState::Preloaded { transaction } => {
                        transaction.clone()
                    }
                    pneumatic_core::transactions::TransactionState::Validated { transaction, .. } => {
                        transaction.clone()
                    }
                    pneumatic_core::transactions::TransactionState::Executing { transaction } => {
                        transaction.clone()
                    }
                    _ => {
                        return Err(ExecutorError::InvalidState(format!(
                            "Transaction {} in terminal state", tx_id
                        )))
                    }
                }
            }
            None => {
                return Err(ExecutorError::Registry(format!(
                    "Transaction {} not in registry", tx_id
                )))
            }
        };

        // Step 2: Transition to Executing state
        {
            if let Some(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
                entry.transition_to_executing(transaction.clone());
            }
        }

        // Step 3: Fetch contract data from DataProvider
        let contract_data = self
            .data_provider
            .get_data(&transaction.token_id, &self.env_id)
            .map_err(ExecutorError::Data)?;

        // Step 4: Fetch user data (sender info) from DataProvider
        let _user_data = self
            .data_provider
            .get_data(&transaction.sender, &self.env_id)
            .map_err(ExecutorError::Data)?;

        // Step 5: Execute the contract with the transaction payload
        let execution_output = self.execute_contract(&transaction, &contract_data)?;

        // Step 6: Create intermediate result
        let mut final_result = ExecutionResult {
            transaction_id: tx_id.to_string(),
            result_data: execution_output.clone(),
            result_hash: vec![],
        };

        // Step 7: Validate execution results
        let validation_result = self.validate_execution_result(&transaction, &final_result);
        if let Err(reasons) = validation_result {
            // Transition to Failed state
            if let Some(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
                entry.transition_to_failed(transaction, reasons.clone());
            }
            return Err(ExecutorError::Validation(reasons));
        }

        // Step 8: Hash the execution output
        final_result.result_hash = self.hash_provider.hash(&execution_output);

        // Step 9: Get finalizer key from validation result
        let finalizer_key = self.get_finalizer_key(tx_id);

        // Step 10: Transition to Finalizing state
        {
            if let Some(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
                entry.transition_to_finalizing(transaction.clone(), finalizer_key.clone());
            }
        }

        // Step 11: Send execution result to the Finalizer
        if let Err(e) = self
            .send_to_finalizer(tx_id, final_result.result_data.clone(), final_result.result_hash.clone(), &finalizer_key)
            .await
        {
            // Transition to Failed state on send failure
            if let Some(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
                entry.transition_to_failed(
                    transaction.clone(),
                    vec![ValidationFailureReason::ContractNotFound],
                );
            }
            return Err(e);
        }

        Ok(final_result)
    }

    /// Execute contract logic with the transaction payload.
    ///
    /// In production, this would:
    /// - Decode the contract bytecode/ABI from `contract_data`
    /// - Run the contract with the transaction as input
    /// - Return the execution output
    ///
    /// Currently a stub — returns the transaction body as the "execution output".
    fn execute_contract(
        &self,
        _tx: &Transaction,
        _contract_data: &[u8],
    ) -> Result<Vec<u8>, ExecutorError> {
        // Stub: serialize the transaction as the execution result.
        // Production: invoke contract bytecode, return computed output.
        let _ = _contract_data;

        // TODO: decode and execute contract bytecode
        // For now, return a minimal success marker
        Ok(serialize_to_bytes_rmp(_tx).map_err(ExecutorError::Encoding)?)
    }

    /// Get the finalizer public key from the transaction's validation result.
    fn get_finalizer_key(&self, tx_id: &str) -> Vec<u8> {
        if let Some(entry) = self.pending_registry.get_transaction_mut(tx_id) {
            if let pneumatic_core::transactions::TransactionState::Validated { validation, .. } =
                &entry.state
            {
                return validation.finalizer_public_key.clone();
            }
        }
        vec![]
    }

    /// Validate execution results against expected constraints.
    fn validate_execution_result(
        &self,
        _tx: &Transaction,
        result: &ExecutionResult,
    ) -> Result<(), Vec<ValidationFailureReason>> {
        let mut reasons = vec![];

        if result.result_hash.is_empty() {
            reasons.push(ValidationFailureReason::ContractNotFound);
        }

        if !reasons.is_empty() {
            return Err(reasons);
        }

        Ok(())
    }

    /// Send execution result to the Finalizer for signature collection.
    async fn send_to_finalizer(
        &self,
        tx_id: &str,
        execution_result: Vec<u8>,
        result_hash: Vec<u8>,
        finalizer_key: &[u8],
    ) -> Result<(), ExecutorError> {
        let finalizer_nodes = self
            .node_registry
            .get_nodes(&NodeRegistryType::Finalizer)
            .ok_or_else(|| ExecutorError::NoFinalizers("No finalizers registered".to_string()))?;

        if finalizer_nodes.is_empty() {
            return Err(ExecutorError::NoFinalizers(
                "No finalizers registered".to_string(),
            ));
        }

        let msg_body = serialize_to_bytes_rmp(&ExecutionResult {
            transaction_id: tx_id.to_string(),
            result_data: execution_result,
            result_hash,
        }).map_err(|e| ExecutorError::Encoding(e))?;

        let message = Message {
            chain_id: self.env_id.clone(),
            action: String::from("Execute"),
            body: msg_body,
            signature: finalizer_key.to_vec(),
            public_key: self.public_key.clone(),
        };

        let payload = serialize_to_bytes_rmp(&message).map_err(|e| ExecutorError::Encoding(e))?;

        self.node_registry.send_to_all(payload, &NodeRegistryType::Finalizer);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ExecutionResult — output from contract execution
// ---------------------------------------------------------------------------

/// Result of executing a transaction's contract logic.
/// Sent to the Finalizer for signature collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Transaction that was executed
    pub transaction_id: String,
    /// Raw execution output bytes
    pub result_data: Vec<u8>,
    /// SHA-256 hash of the result data
    pub result_hash: Vec<u8>,
}

// ---------------------------------------------------------------------------
// ExecutorError — errors specific to executor operations
// ---------------------------------------------------------------------------

/// Errors that can occur during transaction execution.
#[derive(Debug)]
pub enum ExecutorError {
    /// Data provider failed to fetch contract/user/token data
    Data(DataError),
    /// Serialization/deserialization failure
    Encoding(std::io::Error),
    /// Registry operation failed (transaction not found, wrong state)
    Registry(String),
    /// Transaction was in an invalid state for execution
    InvalidState(String),
    /// Validation failed after execution
    Validation(Vec<ValidationFailureReason>),
    /// No finalizer nodes registered
    NoFinalizers(String),
    /// Backpressure: executor is at max capacity
    AtCapacity {
        max_in_flight: usize,
        current: usize,
    },
}

impl From<DataError> for ExecutorError {
    fn from(e: DataError) -> Self {
        ExecutorError::Data(e)
    }
}

impl From<std::io::Error> for ExecutorError {
    fn from(e: std::io::Error) -> Self {
        ExecutorError::Encoding(e)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pneumatic_core::config::Config;
    use pneumatic_core::crypto::BasicHashProvider;
    use pneumatic_core::data::DefaultDataProvider;
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::node::{NodeRegistryType, NodeType};
    use pneumatic_core::transactions::{PendingTransaction, TransactionState};

    fn make_test_hash_provider() -> Arc<dyn HashProvider> {
        Arc::new(BasicHashProvider::new())
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

    fn make_test_config() -> Config {
        Config {
            public_key: vec![1, 2, 3, 4],
            ip_address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Executor],
            main_environment_id: "test_env".to_string(),
            reconciliation_partition_id: "reconciliation".to_string(),
            environment_metadata: make_test_env_data(),
            type_configs: Arc::new(DashMap::new()),
        }
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

    fn make_test_pending_registry() -> Arc<PendingTransactionRegistry> {
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
        };
        let pending = PendingTransaction::new("test_tx_001".to_string(), TransactionState::Preloaded { transaction: tx });
        let _ = registry.add_transaction("test_tx_001".to_string(), pending);
        registry
    }

    fn make_test_data_provider() -> Arc<dyn DataProvider> {
        Arc::new(DefaultDataProvider::new())
    }

    #[tokio::test]
    async fn test_executor_creation() {
        let node_registry = make_test_node_registry();
        let data_provider = make_test_data_provider();
        let pending_registry = make_test_pending_registry();
        let hash_provider = make_test_hash_provider();

        let executor = Executor::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            10,
        );

        assert!(!executor.is_at_capacity().await);
        assert_eq!(executor.in_flight_count().await, 0);
    }

    #[tokio::test]
    async fn test_executor_at_capacity() {
        let node_registry = make_test_node_registry();
        let data_provider = make_test_data_provider();
        let pending_registry = make_test_pending_registry();
        let hash_provider = make_test_hash_provider();

        let executor = Executor::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            1,
        );

        assert!(!executor.is_at_capacity().await);

        // Manually simulate a task being in-flight
        let task_results = Arc::new(DashMap::new());
        executor.preload_tasks.lock().await.insert("test_tx_001".to_string(), task_results);

        assert!(executor.is_at_capacity().await);
        assert_eq!(executor.in_flight_count().await, 1);
    }

    #[tokio::test]
    async fn test_executor_backpressure_rejects() {
        let node_registry = make_test_node_registry();
        let data_provider = make_test_data_provider();
        let pending_registry = Arc::new(PendingTransactionRegistry::new());
        let hash_provider = make_test_hash_provider();

        // Add a valid preloaded transaction so the capacity check is reached
        let tx = Transaction {
            id: "capacity_tx".to_string(),
            action: "Transfer".to_string(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: vec![],
            receiver: vec![],
            amount: Some(0),
            timestamp: 0,
            result_hash: vec![],
        };
        let pending = PendingTransaction::new("capacity_tx".to_string(), TransactionState::Preloaded { transaction: tx });
        let _ = pending_registry.add_transaction("capacity_tx".to_string(), pending);

        let executor = Executor::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            0, // max_in_flight = 0, always at capacity
        );

        let result = executor.preload_for_transaction("capacity_tx").await;
        assert!(result.is_err());

        if let Err(ExecutorError::AtCapacity { max_in_flight, .. }) = result {
            assert_eq!(max_in_flight, 0);
        } else {
            panic!("Expected AtCapacity error, got {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_executor_rejects_nonexistent_transaction() {
        let node_registry = make_test_node_registry();
        let data_provider = make_test_data_provider();
        let pending_registry = make_test_pending_registry();
        let hash_provider = make_test_hash_provider();

        let executor = Executor::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            100,
        );

        let result = executor.preload_for_transaction("nonexistent_tx").await;
        assert!(result.is_err());
        if let Err(ExecutorError::Registry(msg)) = result {
            assert!(msg.contains("nonexistent_tx"));
        } else {
            panic!("Expected Registry error, got {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_executor_rejects_transaction_in_terminal_state() {
        let node_registry = make_test_node_registry();
        let data_provider = make_test_data_provider();
        let pending_registry = Arc::new(PendingTransactionRegistry::new());
        let hash_provider = make_test_hash_provider();

        let executor = Executor::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            data_provider,
            pending_registry.clone(),
            hash_provider,
            100,
        );

        // Add a transaction in Failed state
        let tx = Transaction {
            id: "failed_tx".to_string(),
            action: "Transfer".to_string(),
            token_id: vec![],
            bid: None,
            sequence_number: 1,
            sender: vec![],
            receiver: vec![],
            amount: Some(0),
            timestamp: 0,
            result_hash: vec![],
        };
        let pending = PendingTransaction::new("failed_tx".to_string(), TransactionState::Failed {
            transaction: tx,
            reasons: vec![],
        });
        let _ = pending_registry.add_transaction("failed_tx".to_string(), pending);

        let result = executor.preload_for_transaction("failed_tx").await;
        assert!(result.is_err());
        if let Err(ExecutorError::Registry(msg)) = result {
            assert!(msg.contains("failed_tx"));
        } else {
            panic!("Expected Registry error for terminal state, got {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_executor_cleanup_removes_task() {
        let node_registry = make_test_node_registry();
        let data_provider = make_test_data_provider();
        let pending_registry = make_test_pending_registry();
        let hash_provider = make_test_hash_provider();

        let executor = Executor::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            10,
        );

        // Manually add a task
        let task_results = Arc::new(DashMap::new());
        executor.preload_tasks.lock().await.insert("cleanup_test".to_string(), task_results);

        assert_eq!(executor.in_flight_count().await, 1);

        executor.preload_cleanup("cleanup_test").await;

        assert_eq!(executor.in_flight_count().await, 0);
    }

    // --- ExecutionResult validation ---

    #[test]
    fn validate_execution_result_empty_hash_fails() {
        let node_registry = make_test_node_registry();
        let data_provider = make_test_data_provider();
        let pending_registry = make_test_pending_registry();
        let hash_provider = make_test_hash_provider();

        let executor = Executor::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            10,
        );

        let tx = Transaction {
            id: "test_tx_001".into(),
            action: "Transfer".into(),
            token_id: vec![0, 1, 2],
            bid: None,
            sequence_number: 1,
            sender: vec![10, 20, 30],
            receiver: vec![40, 50, 60],
            amount: Some(100),
            timestamp: 1000,
            result_hash: vec![],
        };
        let result = ExecutionResult {
            transaction_id: "test_tx_001".to_string(),
            result_data: vec![1, 2, 3],
            result_hash: vec![], // empty hash should fail validation
        };
        let validation = executor.validate_execution_result(&tx, &result);
        assert!(validation.is_err());
        let reasons = validation.unwrap_err();
        // Verify ContractNotFound is among the reasons by matching display
        let reason_str = format!("{:?}", reasons);
        assert!(reason_str.contains("ContractNotFound"));
    }

    #[test]
    fn validate_execution_result_nonempty_hash_succeeds() {
        let node_registry = make_test_node_registry();
        let data_provider = make_test_data_provider();
        let pending_registry = make_test_pending_registry();
        let hash_provider = make_test_hash_provider();

        let executor = Executor::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            10,
        );

        let tx = Transaction {
            id: "test_tx_001".into(),
            action: "Transfer".into(),
            token_id: vec![0, 1, 2],
            bid: None,
            sequence_number: 1,
            sender: vec![10, 20, 30],
            receiver: vec![40, 50, 60],
            amount: Some(100),
            timestamp: 1000,
            result_hash: vec![],
        };
        let result = ExecutionResult {
            transaction_id: "test_tx_001".to_string(),
            result_data: vec![1, 2, 3],
            result_hash: vec![9, 8, 7, 6], // non-empty hash
        };
        let validation = executor.validate_execution_result(&tx, &result);
        assert!(validation.is_ok());
    }

    // --- Full backpressure cycle ---

    #[tokio::test]
    async fn full_backpressure_cycle() {
        let node_registry = make_test_node_registry();
        let data_provider = make_test_data_provider();
        let pending_registry = Arc::new(PendingTransactionRegistry::new());
        let hash_provider = make_test_hash_provider();

        // Add two transactions to the registry
        for tx_id in ["bp_tx_a", "bp_tx_b"] {
            let tx = Transaction {
                id: tx_id.to_string(),
                action: "Transfer".to_string(),
                token_id: vec![1],
                bid: None,
                sequence_number: 1,
                sender: vec![],
                receiver: vec![],
                amount: Some(0),
                timestamp: 0,
                result_hash: vec![],
            };
            let pending = PendingTransaction::new(
                tx_id.to_string(),
                TransactionState::Preloaded { transaction: tx },
            );
            let _ = pending_registry.add_transaction(tx_id.to_string(), pending);
        }

        let executor = Executor::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            1, // capacity of 1
        );

        // First preload should succeed (slot available)
        let result_a = executor.preload_for_transaction("bp_tx_a").await;
        assert!(result_a.is_ok());
        assert_eq!(executor.in_flight_count().await, 1);
        assert!(executor.is_at_capacity().await);

        // Second preload should fail due to backpressure
        let result_b = executor.preload_for_transaction("bp_tx_b").await;
        assert!(result_b.is_err());
        if let Err(ExecutorError::AtCapacity { max_in_flight, .. }) = result_b {
            assert_eq!(max_in_flight, 1);
        } else {
            panic!("Expected AtCapacity error, got {:?}", result_b);
        }

        // Cleanup the first task frees a slot
        executor.preload_cleanup("bp_tx_a").await;
        assert_eq!(executor.in_flight_count().await, 0);
        assert!(!executor.is_at_capacity().await);

        // Now the second preload should succeed
        let result_b = executor.preload_for_transaction("bp_tx_b").await;
        assert!(result_b.is_ok());
        assert_eq!(executor.in_flight_count().await, 1);
    }
}
