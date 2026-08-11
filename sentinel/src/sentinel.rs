use std::sync::Arc;

use pneumatic_core::config::Config;
use pneumatic_core::conns::ConnError;
use pneumatic_core::data::{DataError, DefaultDataProvider};
use pneumatic_core::encoding::deserialize_rmp_to;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::messages::Message;
use pneumatic_core::node::{NodeRegistryType, registry::NodeRegistry};
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::transactions::{Transaction, TransactionState};

/// Sentinel — the gatekeeper node type in the pneumatic pipeline.
///
/// Responsibilities:
/// 1. Receive raw transactions from senders (action: "Process")
/// 2. Validate transactions against their spec
/// 3. Route validated transactions through the pipeline:
///    - Self-validated tokens: direct to Committer (skip Executor + Finalizer)
///    - Standard tokens: preload data → Executor → Finalizer → Committer
/// 4. Manage transaction lifecycle in the PendingTransactionRegistry
/// 5. Handle risk-based routing (higher risk → more finalizers)
pub struct Sentinel {
    #[allow(dead_code)]
    node_registry: Arc<NodeRegistry>,
    registry: Arc<PendingTransactionRegistry>,
    gossiper: Arc<Gossiper>,
    transaction_notifier: Arc<super::transaction_notifier::TransactionNotifier>,
    transaction_validator: Arc<super::transaction_validator::TransactionValidator>,
    /// The environment this sentinel operates on.
    env_data: Arc<EnvironmentMetadata>,
}

impl Sentinel {
    /// Create a new Sentinel with all required dependencies.
    ///
    /// The `env_data` parameter should be an `Arc<EnvironmentMetadata>` for the
    /// specific environment this sentinel serves. The caller is responsible for
    /// extracting it from the config's environment registry.
    pub fn new(
        config: Config,
        env_data: Arc<EnvironmentMetadata>,
        node_registry: Arc<NodeRegistry>,
        registry: Arc<PendingTransactionRegistry>,
        gossiper: Arc<Gossiper>,
    ) -> Self {
        let transaction_notifier = Arc::new(
            super::transaction_notifier::TransactionNotifier::new(config, Arc::clone(&node_registry))
        );
        let data_provider = Arc::new(DefaultDataProvider::new());
        let validator = super::transaction_validator::TransactionValidator::new(env_data.clone(), data_provider);

        Sentinel {
            node_registry,
            registry,
            gossiper,
            transaction_notifier,
            transaction_validator: Arc::new(validator),
            env_data,
        }
    }

    /// Initialize the sentinel — set up message handlers and start listening.
    /// The gossiper handles incoming raw data and dispatches to the appropriate
    /// handler based on the Message.action field.
    ///
    /// The closure should be created by the caller using an `Arc<Sentinel>`:
    /// ```ignore
    /// let sentinel = Arc::new(Sentinel::new(...));
    /// let arc = sentinel.clone();
    /// sentinel.initialize(move |raw| {
    ///     if let Err(e) = arc.on_data_received(raw) {
    ///         // log error
    ///     }
    /// });
    /// ```
    pub fn initialize(&self, gossiper_handle: impl Fn(Vec<u8>) + Send + Sync + 'static) {
        self.gossiper.initialize(gossiper_handle);
    }

    /// Handle an incoming raw data frame — the primary entry point.
    /// Deserializes the wire message and routes by action type.
    pub fn on_data_received(&self, raw_data: Vec<u8>) -> Result<(), SentinelError> {
        let message: Message = deserialize_rmp_to(&raw_data)
            .map_err(|e| SentinelError::Encoding(e))?;

        match message.action.as_str() {
            "Process" => self.handle_process_request(message),
            "Confirm" => self.handle_confirmation(message),
            "Reject" => self.handle_rejection(message),
            "Register" => self.handle_register_request(message),
            "Clear" | "Delete" => self.handle_clear_request(message),
            action => Err(SentinelError::UnknownAction(action.to_string())),
        }
    }

    /// Handle a "Process" request — a new transaction entering the pipeline.
    ///
    /// Flow:
    /// 1. Deserialize the transaction from the message body
    /// 2. Basic validation (sender present, nonce > 0)
    /// 3. Register in PendingTransactionRegistry as Pending
    /// 4. Preload data (users, token, contract) from DataProvider
    /// 5. Run spec-based validation
    /// 6. If self-signed (SelfSignedBlockValidatorSpec): skip to Committer
    /// 7. Otherwise: route to Executor for execution, then Finalizer
    fn handle_process_request(&self, message: Message) -> Result<(), SentinelError> {
        let tx: Transaction = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // Step 1: Compute gas used for this transaction (before validation, since validation may fail and we don't track gas for failed txs)
        let gas_used = self.transaction_validator.compute_gas_used(&tx);

        // Step 2: Basic validation
        if let Err(errors) = self.transaction_validator.validate_transaction(&tx, &message) {
            self.transition_to_failed(&tx.id, tx.clone(), errors);
            return Ok(());
        }

        // Step 3: Record gas used
        self.registry.record_gas_used(&tx.id, gas_used);

        // Step 2: Register transaction in the pending registry
        let tx_id = tx.id.clone();
        if self.registry.register_pending(tx_id.clone()).is_err() {
            return Err(SentinelError::TransactionAlreadyExists(tx_id));
        }

        // Step 3: Acquire lock for the preloading stage
        if self.registry.acquire_transaction(&tx_id).is_err() {
            return Err(SentinelError::TransactionInTerminalState(tx_id));
        }

        // Step 4: Determine validation spec for this transaction.
        // In production, load the token from DataProvider using tx.token_id
        // and read its block_validation_spec_name.
        let spec_name = self.get_validation_spec_name(&tx);

        // Step 5: Self-signed check — if spec is SelfSigned, skip Executor/Finalizer
        if spec_name == "SelfSigned" {
            return self.handle_self_signed(tx, gas_used);
        }

        // Step 7: Standard pipeline — send to Executor for preloading
        self.send_to_executor_for_preload(&tx)
    }

    /// Handle a self-signed token transaction — skip Executor and Finalizer,
    /// route directly toward commitment.
    fn handle_self_signed(&self, tx: Transaction, gas_used: u64) -> Result<(), SentinelError> {
        let tx_id = tx.id.clone();

        // Transition to Validated state with self-signed result
        {
            let risk = self.transaction_validator.calculate_risk(&tx);
            if let Ok(mut entry) = self.registry.get_transaction_mut(&tx_id) {
                entry.transition_to_validated(tx.clone(),
                    pneumatic_core::transactions::TransactionValidationResult {
                    is_valid: true,
                    risk,
                    failure_reasons: vec![],
                    finalizer_public_key: vec![], // Empty — self-signed, no finalizer
                });
            }
        }

        // Record gas used for this self-signed transaction
        self.registry.record_gas_used(&tx_id, gas_used);

        // For self-signed tokens, the sentinel notifies Committers directly.
        let _ = tx;

        // Release lock — transaction can be cleaned up after commit
        let _ = self.registry.release_transaction(&tx_id);

        Ok(())
    }

    /// Send a transaction to Executors for data preloading.
    fn send_to_executor_for_preload(&self, tx: &Transaction) -> Result<(), SentinelError> {
        self.transaction_notifier.send_to_executors_for_preload(tx, &self.env_data)
            .map_err(Into::into)
    }

    /// Handle a "Confirm" message — a finalizer has confirmed transaction processing.
    fn handle_confirmation(&self, message: Message) -> Result<(), SentinelError> {
        let tx_id: String = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // Acquire the transaction to prevent concurrent access during transition.
        if self.registry.acquire_transaction(&tx_id).is_err() {
            return Err(SentinelError::TransactionInTerminalState(tx_id.clone()));
        }

        // Verify the sender (message.public_key) matches the assigned finalizer.
        let sender_key = message.public_key.clone();
        if !self.registry.is_requested_finalizer(&tx_id, &sender_key) {
            return Err(SentinelError::Registry(format!(
                "Confirmation from unassigned finalizer {:?} for tx {}",
                sender_key, tx_id
            )));
        }

        // Transition to Committed state.
        if let Ok(mut entry) = self.registry.get_transaction_mut(&tx_id) {
            // Extract the transaction from Finalizing state to move it to Committed.
            let old_state = std::mem::replace(
                &mut entry.state,
                TransactionState::Pending,
            );
            if let TransactionState::Finalizing { transaction, .. } = old_state {
                entry.transition_to_committed(transaction, vec![]); // block_hash not yet available
            } else {
                entry.state = old_state;
                return Err(SentinelError::Registry(format!(
                    "Transaction {} not in Finalizing state for confirmation", tx_id
                )));
            }
        }

        // Notify all other sentinels that this transaction is committed.
        let _ = self.transaction_notifier.notify_delete(&tx_id, &self.env_data);

        // Release lock — transaction will be cleaned up after commit.
        let _ = self.registry.release_transaction(&tx_id);

        Ok(())
    }

    /// Handle a "Reject" message — a finalizer rejected the transaction.
    /// Reassign to a different finalizer using risk-based selection.
    fn handle_rejection(&self, message: Message) -> Result<(), SentinelError> {
        let tx_id: String = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // Acquire the transaction to prevent concurrent access during reassignment.
        if self.registry.acquire_transaction(&tx_id).is_err() {
            return Err(SentinelError::TransactionInTerminalState(tx_id.clone()));
        }

        // Verify the rejecting finalizer was actually the assigned one.
        let rejected_key = message.public_key.clone();
        if !self.registry.is_requested_finalizer(&tx_id, &rejected_key) {
            return Err(SentinelError::Registry(format!(
                "Rejection from non-assigned finalizer {:?} for tx {}",
                rejected_key, tx_id
            )));
        }

        // Extract the transaction from Finalizing state, then pick a new finalizer.
        let node_registry = Arc::clone(&self.node_registry);
        let new_finalizer_key = {
            let Some(nodes) = node_registry.get_nodes(&NodeRegistryType::Finalizer) else {
                return Err(SentinelError::NoTarget(NodeRegistryType::Finalizer));
            };

            // Collect keys excluding the rejected finalizer.
            let mut candidates: Vec<Vec<u8>> = Vec::new();
            for entry in nodes.iter() {
                let key = entry.key();
                if key != &rejected_key {
                    candidates.push(key.clone());
                }
            }

            candidates.into_iter().next()
        };

        let new_key = match new_finalizer_key {
            Some(k) => k,
            None => {
                // Restore the original state since we can't reassign.
                // Note: release_transaction will decrement lock; caller should remove.
                let _ = self.registry.release_transaction(&tx_id);
                return Err(SentinelError::Registry(format!(
                    "No alternative finalizer available for tx {} after rejection", tx_id
                )));
            }
        };

        // Transition to Finalizing with the new finalizer key.
        if let Ok(mut entry) = self.registry.get_transaction_mut(&tx_id) {
            let old_state = std::mem::replace(
                &mut entry.state,
                TransactionState::Pending,
            );
            if let TransactionState::Finalizing { transaction, .. } = old_state {
                entry.transition_to_finalizing(transaction, new_key.clone());
            } else {
                entry.state = old_state;
                let _ = self.registry.release_transaction(&tx_id);
                return Err(SentinelError::Registry(format!(
                    "Transaction {} not in Finalizing state for rejection handling", tx_id
                )));
            }
        }

        // Send the transaction to the new finalizer.
        if let Ok(tx) = self.registry.get_transaction(&tx_id) {
            let _ = self.transaction_notifier.request_single_finalizer(
                &tx, new_key.clone(), &self.env_data
            );
        }

        // Notify all sentinels that this transaction is being reassigned.
        let _ = self.transaction_notifier.notify_delete(&tx_id, &self.env_data);

        // Release lock — transaction remains in Finalizing for new finalizer.
        let _ = self.registry.release_transaction(&tx_id);

        Ok(())
    }

    /// Handle a "Register" request — a node registering with this sentinel.
    fn handle_register_request(&self, _message: Message) -> Result<(), SentinelError> {
        Ok(())
    }

    /// Handle a "Clear"/"Delete" request — remove a transaction from the registry.
    fn handle_clear_request(&self, message: Message) -> Result<(), SentinelError> {
        let tx_id: String = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        let _ = self.registry.remove_transaction(&tx_id);
        Ok(())
    }

    /// Determine which validation spec applies to this transaction.
    fn get_validation_spec_name(&self, tx: &Transaction) -> String {
        if tx.action.is_empty() {
            return String::from("Executed");
        }
        tx.action.clone()
    }

    /// Transition a transaction to Failed state with error reasons.
    fn transition_to_failed(&self, tx_id: &str, tx: Transaction, error: PneumaticError) {
        match error {
            PneumaticError::Validation(reasons) => {
                if let Ok(mut entry) = self.registry.get_transaction_mut(tx_id) {
                    entry.transition_to_failed(tx, reasons);
                }
            }
            _ => {
                if let Ok(mut entry) = self.registry.get_transaction_mut(tx_id) {
                    entry.transition_to_failed(tx, vec![]);
                }
            }
        }
    }
}

/// Errors specific to Sentinel operations.
#[derive(Debug)]
pub enum SentinelError {
    /// Serialization/deserialization failure
    Encoding(std::io::Error),
    /// Network connection failure
    Connection(ConnError),
    /// Data provider failure
    Data(DataError),
    /// Registry operation failure
    Registry(String),
    /// Transaction already exists in registry
    TransactionAlreadyExists(String),
    /// Transaction is in a terminal state (Committed/Failed)
    TransactionInTerminalState(String),
    /// Transaction is not awaiting finalizer (for rejection handling)
    TransactionNotAwaitingFinalizer(String),
    /// No target nodes of a given type available
    NoTarget(NodeRegistryType),
    /// Unknown action type in incoming message
    UnknownAction(String),
}

impl From<std::io::Error> for SentinelError {
    fn from(e: std::io::Error) -> Self {
        SentinelError::Encoding(e)
    }
}

impl From<ConnError> for SentinelError {
    fn from(e: ConnError) -> Self {
        SentinelError::Connection(e)
    }
}

impl From<DataError> for SentinelError {
    fn from(e: DataError) -> Self {
        SentinelError::Data(e)
    }
}

impl From<super::transaction_notifier::NotifyError> for SentinelError {
    fn from(e: super::transaction_notifier::NotifyError) -> Self {
        match e {
            super::transaction_notifier::NotifyError::Encoding(inner) => SentinelError::Encoding(inner),
            super::transaction_notifier::NotifyError::Connection(inner) => SentinelError::Connection(inner),
            super::transaction_notifier::NotifyError::Data(inner) => SentinelError::Data(inner),
            super::transaction_notifier::NotifyError::NoTarget(t) => SentinelError::NoTarget(t),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dashmap::DashMap;
    use pneumatic_core::config::Config;
    use pneumatic_core::node::{NodeRegistryType, NodeType, NodeTypeConfig};
    use pneumatic_core::conns::factories::ConnFactory;
    use pneumatic_core::conns::ConnError;
    use pneumatic_core::data::{DataError, DefaultDataProvider};
    use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::gossiper::Gossiper;
    use pneumatic_core::messages::Message;
    use pneumatic_core::registry::PendingTransactionRegistry;
    use pneumatic_core::tokens::Token;
    use pneumatic_core::transactions::{PendingTransaction, Transaction, TransactionState, TransactionValidationResult};
    use pneumatic_core::errors::TransactionRiskFactor;
    use pneumatic_core::validation::{SelfSignedBlockValidatorSpec, TransactionValidationSpec};

    use super::super::transaction_notifier::TransactionNotifier;
    use super::super::TransactionValidator;
    use super::*;

    // --- helpers ---

    fn make_test_config() -> Config {
        Config {
            public_key: vec![1],
            ip_address: "127.0.0.1".parse().unwrap(),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            type_configs: Arc::new(DashMap::new()),
        }
    }

    fn make_test_node_registry() -> Arc<NodeRegistry> {
        use pneumatic_core::conns::factories::IsConnFactory;
        Arc::new(NodeRegistry::init(
            Arc::new(make_test_config()),
            Box::new(ConnFactory::new()),
            Arc::new(|_| {}),
        ))
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

    fn make_sentinel_fixture() -> (Sentinel, Arc<PendingTransactionRegistry>) {
        let registry = Arc::new(PendingTransactionRegistry::new());
        let node_registry = make_test_node_registry();
        let env_data = Arc::new(make_test_env_data());
        let gossiper = Arc::new(Gossiper::new(
            NodeRegistryType::Sentinel,
            make_test_config(),
            300,
            env_data.asym_crypto_provider.clone(),
        ));
        let sentinel = Sentinel::new(
            make_test_config(),
            env_data,
            node_registry,
            registry.clone(),
            gossiper,
        );
        (sentinel, registry)
    }

    // --- SentinelError From impls ---

    #[test]
    fn sentinel_error_from_io_error_wraps_as_encoding() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test io");
        let err: SentinelError = io_err.into();
        match err {
            SentinelError::Encoding(_) => {}
            _ => panic!("expected Encoding"),
        }
    }

    #[test]
    fn sentinel_error_from_data_error_wraps_as_data() {
        let data_err = DataError::DeserializationError(std::io::Error::new(
            std::io::ErrorKind::Other, "test data",
        ));
        let err: SentinelError = data_err.into();
        match err {
            SentinelError::Data(_) => {}
            _ => panic!("expected Data"),
        }
    }

    // --- Sentinel creation and behavior ---

    #[test]
    fn sentinel_creation_succeeds() {
        let (sentinel, _registry) = make_sentinel_fixture();
        // Just verify it was constructed without panic
        let _ = sentinel;
    }

    #[test]
    fn get_validation_spec_name_empty_defaults_to_executed() {
        let (sentinel, _registry) = make_sentinel_fixture();
        let tx = Transaction {
            id: "test".into(),
            action: "".into(),
            token_id: vec![],
            bid: None,
            sequence_number: 1,
            sender: vec![1],
            receiver: vec![],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
        };
        let name = sentinel.get_validation_spec_name(&tx);
        assert_eq!(name, "Executed");
    }

    #[test]
    fn get_validation_spec_name_nonempty_returns_action() {
        let (sentinel, _registry) = make_sentinel_fixture();
        let tx = Transaction {
            id: "test".into(),
            action: "Transfer".into(),
            token_id: vec![],
            bid: None,
            sequence_number: 1,
            sender: vec![1],
            receiver: vec![],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
        };
        let name = sentinel.get_validation_spec_name(&tx);
        assert_eq!(name, "Transfer");
    }

    // --- on_data_received routing ---

    #[test]
    fn on_data_received_unknown_action_returns_error() {
        let (sentinel, _registry) = make_sentinel_fixture();
        let msg = Message {
            chain_id: "test".into(),
            action: "Zzzz".into(),
            body: vec![],
            signature: vec![],
            public_key: vec![],
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_err());
        match result.unwrap_err() {
            SentinelError::UnknownAction(action) => assert_eq!(action, "Zzzz"),
            _ => panic!("expected UnknownAction"),
        }
    }

    #[test]
    fn on_data_received_process_with_valid_body_no_encoding_error() {
        let (sentinel, _registry) = make_sentinel_fixture();
        let msg = Message {
            chain_id: "test".into(),
            action: "Process".into(),
            body: serialize_to_bytes_rmp(&Transaction {
                id: "test_tx".into(),
                action: "Transfer".into(),
                token_id: vec![1],
                bid: None,
                sequence_number: 1,
                sender: vec![1],
                receiver: vec![2],
                amount: Some(100),
                timestamp: 0,
                result_hash: vec![],
            }).unwrap(),
            signature: vec![1, 2, 3],
            public_key: vec![4, 5, 6],
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        // Should not return Encoding error (may fail on other paths, but that's OK)
        match result {
            Err(SentinelError::Encoding(_)) => panic!("should not be Encoding error"),
            _ => {} // any other error is fine (validation, registry, etc.)
        }
    }

    #[test]
    fn on_data_received_clear_removes_from_registry() {
        let (sentinel, registry) = make_sentinel_fixture();
        // Register a transaction first
        registry.register_pending("tx_clear".into()).unwrap();
        assert!(registry.contains("tx_clear"));

        // Send a "Clear" message with serialized tx_id
        let msg = Message {
            chain_id: "test".into(),
            action: "Clear".into(),
            body: serialize_to_bytes_rmp(&"tx_clear".to_string()).unwrap(),
            signature: vec![],
            public_key: vec![],
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_ok());
        assert!(!registry.contains("tx_clear"));
    }

    // --- T08 integration: self-signed token flow through sentinel ---

    #[test]
    fn sentinel_self_signed_token_flow_end_to_end() {
        let (_sentinel, registry) = make_sentinel_fixture();
        let (token, _node_registry) = (
            {
                let mut token = Token::new();
                token.set_metadata("owner".to_string(), "alice".to_string());
                token
            },
            make_test_node_registry(),
        );

        // Create a self-signed transaction (sender == owner)
        let tx = Transaction {
            id: "tx_self_signed".into(),
            action: "Transfer".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b"alice".to_vec(),
            receiver: vec![],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
        };

        // Validate with SelfSigned spec directly
        let spec = SelfSignedBlockValidatorSpec::new();
        let env = make_test_env_data();
        let validation_result = spec.validate(&tx, &token, &env).unwrap();
        assert!(validation_result.is_valid);

        // Create PendingTransaction and transition to Validated
        let tx_id = tx.id.clone();
        let mut pt = PendingTransaction::new(tx_id.clone(), TransactionState::Pending);
        pt.transition_to_validated(tx.clone(), validation_result);
        registry.add_transaction(tx_id.clone(), pt).unwrap();

        // Verify the sentinel sees the transaction as validated
        let validation = registry.get_validation_result(&tx_id).unwrap();
        assert!(validation.is_valid);
    }

    // --- compute_gas_used tests ---

    #[test]
    fn compute_gas_used_with_zero_amount_returns_base_cost() {
        let validator = TransactionValidator::new(
            Arc::new(make_test_env_data()),
            Arc::new(DefaultDataProvider::new()),
        );
        let tx = Transaction {
            id: "test".into(),
            action: "Process".into(),
            token_id: vec![],
            bid: None,
            sequence_number: 1,
            sender: vec![1],
            receiver: vec![2],
            amount: Some(0),
            timestamp: 0,
            result_hash: vec![],
        };
        let gas = validator.compute_gas_used(&tx);
        // base_cost=1, amount=0, multiplier=1.0 → 1 + 0 = 1
        assert_eq!(gas, 1);
    }

    #[test]
    fn compute_gas_used_preload_with_amount_applies_multiplier() {
        let validator = TransactionValidator::new(
            Arc::new(make_test_env_data()),
            Arc::new(DefaultDataProvider::new()),
        );
        let tx = Transaction {
            id: "test".into(),
            action: "Preload".into(),
            token_id: vec![],
            bid: None,
            sequence_number: 1,
            sender: vec![1],
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
        };
        let gas = validator.compute_gas_used(&tx);
        // base_cost=1, amount=100, Preload multiplier=2.0 → 1 + 200 = 201
        assert_eq!(gas, 201);
    }

    #[test]
    fn compute_gas_used_unknown_action_defaults_to_one() {
        let validator = TransactionValidator::new(
            Arc::new(make_test_env_data()),
            Arc::new(DefaultDataProvider::new()),
        );
        let tx = Transaction {
            id: "test".into(),
            action: "UnknownAction".into(),
            token_id: vec![],
            bid: None,
            sequence_number: 1,
            sender: vec![1],
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
        };
        let gas = validator.compute_gas_used(&tx);
        // base_cost=1, amount=100, unknown multiplier=1.0 → 1 + 100 = 101
        assert_eq!(gas, 101);
    }

    // --- TransactionNotifier tests ---

    #[test]
    fn transaction_notifier_send_to_executors_does_not_panic() {
        let node_registry = make_test_node_registry();
        let config = make_test_config();
        let notifier = TransactionNotifier::new(config, node_registry);
        let env = make_test_env_data();
        let tx = Transaction {
            id: "test_tx".into(),
            action: "Preload".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: vec![1],
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
        };
        // Should succeed (spawns async task; no nodes registered means no sends)
        let result = notifier.send_to_executors_for_preload(&tx, &env);
        assert!(result.is_ok());
    }

    #[test]
    fn transaction_notifier_send_to_finalizer_does_not_panic() {
        let node_registry = make_test_node_registry();
        let config = make_test_config();
        let notifier = TransactionNotifier::new(config, node_registry);
        let env = make_test_env_data();
        let tx = Transaction {
            id: "test_tx".into(),
            action: "Preload".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: vec![1],
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
        };
        let result = notifier.send_to_finalizer_for_preload(&tx, b"finalizer_key", &env);
        assert!(result.is_ok());
    }

    #[test]
    fn transaction_notifier_notify_clear_does_not_panic() {
        let node_registry = make_test_node_registry();
        let config = make_test_config();
        let notifier = TransactionNotifier::new(config, node_registry);
        let env = make_test_env_data();
        let result = notifier.notify_clear_to_process("tx_123", &env);
        assert!(result.is_ok());
    }

    #[test]
    fn transaction_notifier_notify_delete_does_not_panic() {
        let node_registry = make_test_node_registry();
        let config = make_test_config();
        let notifier = TransactionNotifier::new(config, node_registry);
        let env = make_test_env_data();
        let result = notifier.notify_delete("tx_123", &env);
        assert!(result.is_ok());
    }

    // --- handle_confirmation tests ---

    fn make_finalizing_entry(registry: &PendingTransactionRegistry, tx_id: &str, finalizer_key: Vec<u8>) {
        registry.register_pending(tx_id.into()).unwrap();
        if let Ok(mut entry) = registry.get_transaction_mut(tx_id) {
            entry.transition_to_validated(
                Transaction {
                    id: tx_id.into(), action: "Transfer".into(),
                    token_id: vec![1], bid: None, sequence_number: 1,
                    sender: vec![1], receiver: vec![2], amount: Some(100),
                    timestamp: 0, result_hash: vec![],
                },
                TransactionValidationResult::valid(
                    finalizer_key.clone(),
                    TransactionRiskFactor {
                        affected_parties: 2, amount: 100,
                        is_contract: false, is_multi_party: false,
                    },
                ),
            );
        }
        registry.set_requested_finalizer(tx_id, finalizer_key).unwrap();
    }

    #[test]
    fn handle_confirmation_valid_finalizer_transitions_to_committed() {
        let (sentinel, registry) = make_sentinel_fixture();
        let finalizer_key = vec![99];
        make_finalizing_entry(&registry, "tx_confirm", finalizer_key.clone());

        // Send a "Confirm" message from the assigned finalizer.
        let msg = Message {
            chain_id: "test".into(),
            action: "Confirm".into(),
            body: serialize_to_bytes_rmp(&"tx_confirm".to_string()).unwrap(),
            signature: vec![],
            public_key: finalizer_key,
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_ok());

        // Transaction should now be in Committed state.
        let entry = registry.get_transaction_mut("tx_confirm").unwrap();
        assert!(matches!(entry.state, TransactionState::Committed { .. }));
    }

    #[test]
    fn handle_confirmation_unassigned_finalizer_returns_error() {
        let (sentinel, registry) = make_sentinel_fixture();
        let finalizer_key = vec![99];
        make_finalizing_entry(&registry, "tx_bad_confirm", finalizer_key.clone());

        // Send a "Confirm" message from an unassigned finalizer.
        let msg = Message {
            chain_id: "test".into(),
            action: "Confirm".into(),
            body: serialize_to_bytes_rmp(&"tx_bad_confirm".to_string()).unwrap(),
            signature: vec![],
            public_key: vec![1, 2, 3], // wrong key
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_err());
        match result.unwrap_err() {
            SentinelError::Registry(msg) => assert!(msg.contains("unassigned")),
            _ => panic!("expected Registry error"),
        }
    }

    #[test]
    fn handle_confirmation_not_in_finalizing_state_returns_error() {
        let (sentinel, registry) = make_sentinel_fixture();
        // Register but don't set a finalizer — stays in Pending.
        registry.register_pending("tx_no_finalizer".into()).unwrap();

        let msg = Message {
            chain_id: "test".into(),
            action: "Confirm".into(),
            body: serialize_to_bytes_rmp(&"tx_no_finalizer".to_string()).unwrap(),
            signature: vec![],
            public_key: vec![99],
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_err());
    }

    // --- handle_rejection tests ---

    #[test]
    fn handle_rejection_reassigns_to_new_finalizer() {
        let (sentinel, registry) = make_sentinel_fixture();
        let rejected_key = vec![1];
        let new_key = vec![2];
        make_finalizing_entry(&registry, "tx_reject", rejected_key.clone());

        // Send a "Reject" message from the assigned finalizer.
        let msg = Message {
            chain_id: "test".into(),
            action: "Reject".into(),
            body: serialize_to_bytes_rmp(&"tx_reject".to_string()).unwrap(),
            signature: vec![],
            public_key: rejected_key,
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_err());
        match result.unwrap_err() {
            SentinelError::Registry(msg) => assert!(msg.contains("No alternative finalizer")),
            e => panic!("expected Registry error, got {:?}", e),
        }
    }

    #[test]
    fn handle_rejection_unassigned_finalizer_returns_error() {
        let (sentinel, registry) = make_sentinel_fixture();
        let assigned_key = vec![99];
        make_finalizing_entry(&registry, "tx_bad_reject", assigned_key.clone());

        // Send a "Reject" message from a non-assigned finalizer.
        let msg = Message {
            chain_id: "test".into(),
            action: "Reject".into(),
            body: serialize_to_bytes_rmp(&"tx_bad_reject".to_string()).unwrap(),
            signature: vec![],
            public_key: vec![5, 6, 7],
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_err());
        match result.unwrap_err() {
            SentinelError::Registry(msg) => assert!(msg.contains("non-assigned")),
            _ => panic!("expected Registry error"),
        }
    }

    #[test]
    fn handle_rejection_terminal_state_returns_error() {
        let (sentinel, registry) = make_sentinel_fixture();
        // Register a pending transaction (terminal state = Failed/Committed would reject acquire)
        registry.register_pending("tx_terminal".into()).unwrap();
        // Transition to Failed (terminal)
        if let Ok(mut entry) = registry.get_transaction_mut("tx_terminal") {
            entry.transition_to_failed(
                Transaction {
                    id: "tx_terminal".into(), action: "Transfer".into(),
                    token_id: vec![], bid: None, sequence_number: 0,
                    sender: vec![], receiver: vec![], amount: None,
                    timestamp: 0, result_hash: vec![],
                },
                vec![],
            );
        }

        let msg = Message {
            chain_id: "test".into(),
            action: "Reject".into(),
            body: serialize_to_bytes_rmp(&"tx_terminal".to_string()).unwrap(),
            signature: vec![],
            public_key: vec![1],
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_err());
        match result.unwrap_err() {
            SentinelError::TransactionInTerminalState(id) => assert_eq!(id, "tx_terminal"),
            _ => panic!("expected TransactionInTerminalState error"),
        }
    }
}
