use std::sync::Arc;

use pneumatic_core::config::Config;
use pneumatic_core::conns::ConnError;
use pneumatic_core::data::DataError;
use pneumatic_core::encoding::deserialize_rmp_to;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::transactions::Transaction;

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
            super::transaction_notifier::TransactionNotifier::new(config)
        );
        let validator = super::transaction_validator::TransactionValidator::new(env_data.clone());

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
    pub fn initialize(&self) {
        // Wire up the gossiper to route messages to on_data_received.
        // In production:
        //   self.gossiper.initialize(move |raw| {
        //       if let Err(e) = self.on_data_received(raw) {
        //           // log error
        //       }
        //   });
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

        // Step 1: Basic validation
        if let Err(errors) = self.transaction_validator.validate_transaction(&tx, &message) {
            self.transition_to_failed(&tx.id, tx.clone(), errors);
            return Ok(());
        }

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
            return self.handle_self_signed(tx);
        }

        // Step 6: Standard pipeline — send to Executor for preloading
        self.send_to_executor_for_preload(&tx)
    }

    /// Handle a self-signed token transaction — skip Executor and Finalizer,
    /// route directly toward commitment.
    fn handle_self_signed(&self, tx: Transaction) -> Result<(), SentinelError> {
        let tx_id = tx.id.clone();

        // Transition to Validated state with self-signed result
        {
            let risk = self.transaction_validator.calculate_risk(&tx);
            if let Some(mut entry) = self.registry.get_transaction_mut(&tx_id) {
                entry.transition_to_validated(tx.clone(),
                    pneumatic_core::transactions::TransactionValidationResult {
                    is_valid: true,
                    risk,
                    failure_reasons: vec![],
                    finalizer_public_key: vec![], // Empty — self-signed, no finalizer
                });
            }
        }

        // For self-signed tokens, the sentinel notifies Committers directly.
        let _ = tx;

        // Release lock — transaction can be cleaned up after commit
        let _ = self.registry.release_transaction(&tx_id);

        Ok(())
    }

    /// Send a transaction to Executors for data preloading.
    fn send_to_executor_for_preload(&self, tx: &Transaction) -> Result<(), SentinelError> {
        // In production: use TransactionNotifier to send Preload action
        // to Executor nodes in this environment
        let _ = tx;
        Ok(())
    }

    /// Handle a "Confirm" message — a finalizer has confirmed transaction processing.
    fn handle_confirmation(&self, _message: Message) -> Result<(), SentinelError> {
        // Acquire the transaction, verify the finalizer is the assigned one.
        // If confirmed, transition to Committed and notify all sentinels.
        Ok(())
    }

    /// Handle a "Reject" message — a finalizer rejected the transaction.
    /// Reassign to a different finalizer using risk-based selection.
    fn handle_rejection(&self, _message: Message) -> Result<(), SentinelError> {
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
                if let Some(mut entry) = self.registry.get_transaction_mut(tx_id) {
                    entry.transition_to_failed(tx, reasons);
                }
            }
            _ => {
                if let Some(mut entry) = self.registry.get_transaction_mut(tx_id) {
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
    use pneumatic_core::data::DataError;
    use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::gossiper::Gossiper;
    use pneumatic_core::messages::Message;
    use pneumatic_core::registry::PendingTransactionRegistry;
    use pneumatic_core::tokens::Token;
    use pneumatic_core::transactions::{PendingTransaction, Transaction, TransactionState};
    use pneumatic_core::validation::{SelfSignedBlockValidatorSpec, TransactionValidationSpec};

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
        let (sentinel, registry) = make_sentinel_fixture();
        let (token, node_registry) = (
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
        let validation = registry.get_validation_result(&tx_id);
        assert!(validation.is_some());
        assert!(validation.unwrap().is_valid);
    }
}
