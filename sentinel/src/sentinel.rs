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
