use pneumatic_core::config::Config;
use pneumatic_core::conns::ConnError;
use pneumatic_core::data::DataError;
use pneumatic_core::encoding::serialize_to_bytes_rmp;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::messages::Message;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::transactions::Transaction;

/// Handles all outbound message sending from the Sentinel to other node types.
/// Packages transactions and commands into the proper `Message` wire format.
pub struct TransactionNotifier {
    config: Config,
}

impl TransactionNotifier {
    pub fn new(config: Config) -> Self {
        TransactionNotifier { config }
    }

    /// Send a transaction to all Executors for data preloading.
    pub fn send_to_executors_for_preload(
        &self,
        tx: &Transaction,
        env: &EnvironmentMetadata,
    ) -> Result<(), NotifyError> {
        let msg = Message {
            chain_id: env.token_partition_id.clone(),
            action: String::from("Preload"),
            body: serialize_to_bytes_rmp(tx).map_err(NotifyError::Encoding)?,
            signature: vec![],
            public_key: self.config.public_key.clone(),
        };

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        self.send_to_nodes(NodeRegistryType::Executor, payload)
    }

    /// Send a validated transaction to the assigned Finalizer for execution preloading.
    pub fn send_to_finalizer_for_preload(
        &self,
        tx: &Transaction,
        finalizer_key: &[u8],
        env: &EnvironmentMetadata,
    ) -> Result<(), NotifyError> {
        let msg = Message {
            chain_id: env.token_partition_id.clone(),
            action: String::from("Preload"),
            body: serialize_to_bytes_rmp(tx).map_err(NotifyError::Encoding)?,
            signature: finalizer_key.to_vec(),
            public_key: self.config.public_key.clone(),
        };

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        self.send_to_nodes(NodeRegistryType::Finalizer, payload)
    }

    /// Notify all Sentinels to continue processing a cleared transaction.
    pub fn notify_clear_to_process(&self, tx_id: &str, env: &EnvironmentMetadata) -> Result<(), NotifyError> {
        let body = serialize_to_bytes_rmp(&tx_id).map_err(NotifyError::Encoding)?;
        let msg = Message {
            chain_id: env.token_partition_id.clone(),
            action: String::from("Clear"),
            body,
            signature: vec![],
            public_key: self.config.public_key.clone(),
        };

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        self.send_to_nodes(NodeRegistryType::Sentinel, payload)
    }

    /// Notify all Sentinels to delete a transaction from the registry.
    pub fn notify_delete(&self, tx_id: &str, env: &EnvironmentMetadata) -> Result<(), NotifyError> {
        let body = serialize_to_bytes_rmp(&tx_id).map_err(NotifyError::Encoding)?;
        let msg = Message {
            chain_id: env.token_partition_id.clone(),
            action: String::from("Delete"),
            body,
            signature: vec![],
            public_key: self.config.public_key.clone(),
        };

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        self.send_to_nodes(NodeRegistryType::Sentinel, payload)
    }

    /// Request a Finalizer to take responsibility for a validated transaction.
    pub fn request_finalizer(
        &self,
        tx: &Transaction,
        env: &EnvironmentMetadata,
    ) -> Result<Vec<u8>, NotifyError> {
        let msg = Message {
            chain_id: env.token_partition_id.clone(),
            action: String::from("FinalizerRequest"),
            body: serialize_to_bytes_rmp(tx).map_err(NotifyError::Encoding)?,
            signature: vec![],
            public_key: self.config.public_key.clone(),
        };

        let payload = serialize_to_bytes_rmp(&msg).map_err(NotifyError::Encoding)?;
        let _ = self.send_to_nodes(NodeRegistryType::Finalizer, payload)?;
        Ok(vec![])
    }

    /// Broadcast a message to all nodes of a given type in the registry.
    fn send_to_nodes(&self, _target_type: NodeRegistryType, _payload: Vec<u8>) -> Result<(), NotifyError> {
        // Stub: actual networking wired when node registries are injected.
        // Pattern: iterate NodeRegistry entries for target type → get Sender per node → send.
        Ok(())
    }
}

/// Errors specific to transaction notification/sending.
#[derive(Debug)]
pub enum NotifyError {
    Encoding(std::io::Error),
    Connection(ConnError),
    Data(DataError),
    NoTarget(NodeRegistryType),
}

impl From<ConnError> for NotifyError {
    fn from(e: ConnError) -> Self {
        NotifyError::Connection(e)
    }
}

impl From<DataError> for NotifyError {
    fn from(e: DataError) -> Self {
        NotifyError::Data(e)
    }
}
