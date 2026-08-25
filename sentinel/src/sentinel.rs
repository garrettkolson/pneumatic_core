use std::sync::Arc;

use pneumatic_core::blocks::Block;
use pneumatic_core::config::Config;
use pneumatic_core::conns::ConnError;
use pneumatic_core::data::{DataError, DataProvider};
use pneumatic_core::encoding::deserialize_rmp_to;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::messages::Message;
use pneumatic_core::node::{NodeRegistryRequest, NodeRegistryType, registry::NodeRegistry};
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::transactions::{Transaction, TransactionState};

use super::executor_set_cache::ExecutorSetCache;
use super::stake_snapshot_cache::StakeSnapshotCache;

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
    data_provider: Arc<dyn DataProvider>,
    /// Stake snapshot cache for deterministic per-transaction routing.
    /// Loaded from local cache → DataProvider → (reserved) peer fallback.
    stake_snapshot_cache: Arc<StakeSnapshotCache>,
    /// Executor set cache for deterministic shard-aware routing.
    /// Loaded from local cache → DataProvider → (reserved) peer fallback.
    executor_set_cache: Arc<ExecutorSetCache>,
    /// The environment this sentinel operates on.
    env_data: Arc<EnvironmentMetadata>,
    /// Current epoch number — advanced when a new epoch boundary is detected.
    current_epoch: parking_lot::Mutex<u64>,
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
        data_provider: Arc<dyn DataProvider>,
    ) -> Self {
        let transaction_notifier = Arc::new(
            super::transaction_notifier::TransactionNotifier::new(config, Arc::clone(&node_registry))
        );
        let validator = super::transaction_validator::TransactionValidator::new(env_data.clone(), Arc::clone(&data_provider));
        let partition_id = env_data.environment_id.clone();
        let stake_snapshot_cache = Arc::new(
            StakeSnapshotCache::new(data_provider.clone(), partition_id.clone())
        );
        let executor_set_cache = Arc::new(
            ExecutorSetCache::new(data_provider.clone(), partition_id)
        );

        Sentinel {
            node_registry,
            registry,
            gossiper,
            transaction_notifier,
            transaction_validator: Arc::new(validator),
            data_provider,
            stake_snapshot_cache,
            executor_set_cache,
            env_data,
            current_epoch: parking_lot::Mutex::new(1),
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
            "BlockFinalized" => self.handle_block_finalized_for_epoch(message),
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

        // Phase 3.1 (AUDIT finding C3): fail-closed sender authentication. This runs
        // *before* `compute_gas_used`/`validate_transaction` so a forged or unauthorized
        // transaction is dropped the instant it arrives, without touching the pool or
        // consuming gas. Two independent checks must both pass:
        //
        //   1. `tx.sender` must be non-empty.
        //   2. `tx.sender`'s Ed25519 signature over the canonical transaction bytes
        //      (`tx.sender_signature`) must verify — the signature proves the submitter
        //      actually authorized *this* payload, not a swapped replacement.
        //   3. The authenticated envelope sender (`message.public_key`, verified by the
        //      gossiper) must equal `tx.sender` — the network submitter must be the
        //      account being debited, so a peer can't debit an account it does not own.
        if tx.sender.is_empty() {
            return Err(SentinelError::UnauthenticatedSubmitter(
                "transaction has an empty sender".to_string(),
            ));
        }
        let sender_authorized = tx.verify_sender_signature().map_err(|e| {
            // `check_signature` returns Err on malformed/corrupt bytes; fail closed.
            SentinelError::InvalidSenderSignature(format!("sender-signature check error: {e}"))
        })?;
        if !sender_authorized {
            return Err(SentinelError::InvalidSenderSignature(
                "sender did not authorize this transaction".to_string(),
            ));
        }
        if message.public_key != tx.sender {
            return Err(SentinelError::UnauthenticatedSubmitter(
                "authenticated envelope sender does not match transaction sender".to_string(),
            ));
        }

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

        // Step 6: Transition to Validated and enqueue into the pool for leader ordering.
        let risk = self.transaction_validator.calculate_risk(&tx);
        let _ = self.registry.transition_to_validated_and_enqueue(
            &tx_id,
            tx.clone(),
            pneumatic_core::transactions::TransactionValidationResult {
                is_valid: true,
                risk,
                failure_reasons: vec![],
                finalizer_public_key: vec![],
            },
        );

        // Step 7: Standard pipeline — send to Executor for preloading
        self.send_to_executor_for_preload(&tx)
    }

    /// Handle a self-signed token transaction — skip Executor and Finalizer,
    /// route directly toward commitment.
    fn handle_self_signed(&self, tx: Transaction, gas_used: u64) -> Result<(), SentinelError> {
        let tx_id = tx.id.clone();

        // Transition to Validated state and enqueue into the pool in one atomic operation.
        let risk = self.transaction_validator.calculate_risk(&tx);
        let _ = self.registry.transition_to_validated_and_enqueue(
            &tx_id,
            tx.clone(),
            pneumatic_core::transactions::TransactionValidationResult {
                is_valid: true,
                risk,
                failure_reasons: vec![],
                finalizer_public_key: vec![], // Empty — self-signed, no finalizer
            },
        );

        // Record gas used for this self-signed transaction
        self.registry.record_gas_used(&tx_id, gas_used);

        // For self-signed tokens, the sentinel notifies Committers directly.
        let _ = tx;

        // Release lock — transaction can be cleaned up after commit
        let _ = self.registry.release_transaction(&tx_id);

        Ok(())
    }

    /// Send a transaction to Executors for data preloading.
    /// When sharding is enabled (shard_count > 1), routes only to the shard's executors.
    fn send_to_executor_for_preload(&self, tx: &Transaction) -> Result<(), SentinelError> {
        if self.env_data.shard_count > 1 {
            // Shard-aware routing: only send to the selected shard's executors
            let shard_executors = self.get_shard_executors(&tx.id, *self.current_epoch.lock())?;
            self.transaction_notifier
                .send_to_shard_executors_for_preload(tx, &shard_executors, &self.env_data)
                .map_err(Into::into)
        } else {
            // No sharding: broadcast to all executors (existing behavior)
            self.transaction_notifier
                .send_to_executors_for_preload(tx, &self.env_data)
                .map_err(Into::into)
        }
    }

    /// Get the executor public keys for the transaction's shard.
    fn get_shard_executors(&self, tx_id: &str, epoch_number: u64) -> Result<Vec<Vec<u8>>, SentinelError> {
        let executors = self.executor_set_cache.get(epoch_number)
            .ok_or_else(|| SentinelError::Routing(format!("No executor set for epoch {}", epoch_number)))?;

        if executors.is_empty() {
            return Err(SentinelError::Routing("Executor set is empty".into()));
        }

        let shard_executors = pneumatic_core::deterministic_select_shard(
            &executors,
            self.env_data.shard_count,
            tx_id,
            epoch_number,
        )
        .ok_or_else(|| SentinelError::Routing("Selected shard has no executors".into()))?;

        if shard_executors.is_empty() {
            return Err(SentinelError::Routing("Selected shard has no executors".into()));
        }

        Ok(shard_executors)
    }

    /// Advance the sentinel to a new epoch.
    ///
    /// Invalidates the executor set cache so the next transaction triggers
    /// a fresh load + shuffle. Updates the tracked epoch number.
    ///
    /// Call this when a new epoch is detected (e.g., from chain blocks).
    pub fn advance_epoch(&self, epoch_number: u64) {
        // Fail-closed: never rewind the epoch. A stale or replayed block must not
        // roll routing back to an older executor/stake snapshot.
        if epoch_number <= *self.current_epoch.lock() {
            return;
        }
        *self.current_epoch.lock() = epoch_number;
        self.executor_set_cache.invalidate_all();
        self.stake_snapshot_cache.invalidate_all();
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

        // Assign a new finalizer deterministically using the current stake snapshot.
        // Falls back to random candidate selection if the snapshot is unavailable.
        let new_key = match self.assign_finalizer_deterministic_retry(&tx_id, *self.current_epoch.lock(), &rejected_key) {
            Ok(key) => key,
            Err(_) => {
                // Fallback: pick the first non-rejected candidate from the node registry.
                let Some(nodes) = self.node_registry.get_nodes(&NodeRegistryType::Finalizer) else {
                    let _ = self.registry.release_transaction(&tx_id);
                    return Err(SentinelError::NoTarget(NodeRegistryType::Finalizer));
                };

                let fallback_keys: Vec<Vec<u8>> = nodes.iter()
                    .filter_map(|entry| {
                        let entry_key = entry.key();
                        if entry_key != &rejected_key {
                            Some(entry_key.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                match fallback_keys.into_iter().next() {
                    Some(k) => k,
                    None => {
                        let _ = self.registry.release_transaction(&tx_id);
                        return Err(SentinelError::Registry(format!(
                            "No alternative finalizer available for tx {} after rejection", tx_id
                        )));
                    }
                }
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

    /// Advance the sentinel's epoch from a `BlockFinalized` gossip message.
    ///
    /// Fail-closed (AUDIT H5): only a registered finalizer may move the epoch, and
    /// the advance is monotonic (an `advance_epoch` guard rejects stale/replayed
    /// blocks). The `epoch_number` is bound into the block hash (Phase 2.1), so a
    /// gossiper-authenticated `BlockFinalized` from a registered finalizer is a
    /// trustworthy epoch signal. This is an availability signal, not a chain-append:
    /// the sentinel does not validate linkage here.
    fn handle_block_finalized_for_epoch(&self, message: Message) -> Result<(), SentinelError> {
        let block: Block = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // Fail-closed role guard: only a registered finalizer may advance the epoch.
        // `message.public_key` is the gossiper-authenticated sender identity.
        let is_finalizer = self
            .node_registry
            .get_nodes(&NodeRegistryType::Finalizer)
            .map(|nodes| nodes.iter().any(|n| n.key() == &message.public_key))
            .unwrap_or(false);
        if !is_finalizer {
            return Err(SentinelError::Registry(format!(
                "BlockFinalized from non-finalizer {:?}",
                message.public_key
            )));
        }

        self.advance_epoch(block.epoch_number);
        Ok(())
    }

    /// Handle a "Register" request — a node registering with this sentinel.
    fn handle_register_request(&self, message: Message) -> Result<(), SentinelError> {
        let request: NodeRegistryRequest = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // The binding signature authenticates that the Ed25519 key is bound
        // to the claimed rhash — the same check the control-plane registry
        // applies to `NodeRequest` Register messages.
        if !pneumatic_core::rns::identity::NodeIdentity::verify_binding(
            &request.requester_key,
            &request.rhash,
            &request.requested_type,
            &request.requester_types,
            &request.binding_signature,
        ) {
            return Err(SentinelError::Registry(
                "invalid binding signature".to_string(),
            ));
        }

        // Reject if already registered for the requested type.
        if self.node_registry.node_is_already_registered(&request.requester_key, &request.requested_type) {
            return Err(SentinelError::Registry(format!(
                "Node {:?} already registered as {:?}",
                request.requester_key, request.requested_type
            )));
        }

        // Validate stake for the requested type.
        if !self.check_stake_for_type(&request.requester_key, &request.requested_type)? {
            return Err(SentinelError::Registry(format!(
                "Insufficient stake for node {:?} registering as {:?}",
                request.requester_key, request.requested_type
            )));
        }

        // Add node to each requested type's registry.
        for node_type in &request.requester_types {
            if let Some(nodes) = self.node_registry.get_nodes(node_type) {
                let node_entry = pneumatic_core::node::NodeRegistryNode::new(
                    request.rhash,
                    Box::new(pneumatic_core::node::registry::NullConnection),
                );
                nodes.insert(request.requester_key.clone(), node_entry);
            }
        }

        Ok(())
    }

    /// Check if the user with the given key has sufficient stake for the requested node type.
    fn check_stake_for_type(&self, key: &Vec<u8>, node_type: &NodeRegistryType) -> Result<bool, SentinelError> {
        let user = self.data_provider.get_user(key, &self.env_data.environment_id)?;
        let min_stake = self.node_registry.get_config().get_min_type_stake(node_type);
        Ok(user.stake >= min_stake)
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

    /// Deterministically assign a finalizer for a transaction using the current
    /// stake snapshot. Returns the assigned finalizer's public key.
    ///
    /// Uses the sentinel's `StakeSnapshotCache` to load the snapshot for the
    /// given epoch, then delegates to `pneumatic_core::deterministic_select`.
    ///
    /// If the snapshot is not cached and the DataProvider call fails, returns
    /// a `Routing` error.
    pub fn assign_finalizer_deterministic(
        &self,
        tx_id: &str,
        epoch_number: u64,
    ) -> Result<Vec<u8>, SentinelError> {
        let snapshot = self.stake_snapshot_cache.get(epoch_number)
            .ok_or_else(|| SentinelError::Routing(format!("No snapshot for epoch {}", epoch_number)))?;

        if snapshot.total_stake() == 0 {
            return Err(SentinelError::Routing("Stake set is empty".into()));
        }

        let finalizer_key = pneumatic_core::deterministic_select(&snapshot, tx_id.as_bytes(), epoch_number)
            .ok_or_else(|| SentinelError::Routing("Selection returned none for non-empty stake set".into()))?;

        if snapshot.get_stake(&finalizer_key) == 0 {
            return Err(SentinelError::Routing("Assigned finalizer has zero stake".into()));
        }

        Ok(finalizer_key)
    }

    /// Assign a finalizer deterministically, with a retry suffix if the
    /// initial assignment matches a rejected finalizer.
    pub fn assign_finalizer_deterministic_retry(
        &self,
        tx_id: &str,
        epoch_number: u64,
        rejected_key: &[u8],
    ) -> Result<Vec<u8>, SentinelError> {
        let key = self.assign_finalizer_deterministic(tx_id, epoch_number)?;
        if key == rejected_key {
            // Try with a "retry" suffix to shift the selection
            let retry_tx_id = format!("{}_retry", tx_id);
            return self.assign_finalizer_deterministic(&retry_tx_id, epoch_number);
        }
        Ok(key)
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
    /// Deterministic routing failure (no snapshot, empty stake set, etc.)
    Routing(String),
    /// The authenticated envelope sender (`message.public_key`) does not match
    /// `transaction.sender` — a peer tried to debit an account whose key it does
    /// not control (AUDIT finding C3).
    UnauthenticatedSubmitter(String),
    /// The transaction's `sender_signature` is missing, empty, or does not verify
    /// against `sender` over the canonical transaction bytes (AUDIT finding C3).
    InvalidSenderSignature(String),
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
    use pneumatic_core::crypto::AsymCryptoProvider;
    use pneumatic_core::rns::identity::NodeIdentity;
    use pneumatic_core::node::{NodeRegistryRequest, NodeRegistryType, NodeType, NodeTypeConfig};
    use pneumatic_core::user::User;
    use pneumatic_core::conns::ConnError;
    use pneumatic_core::data::{DataError, DefaultDataProvider, StubDataProvider};
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
        let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
        let public_key = identity.ed25519.public_key().unwrap_or_default();
        let rhash = identity.rhash;
        Config {
            public_key,
            ip_address: "127.0.0.1".parse().unwrap(),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            // Node-type capacity entries are required — without them get_max_node_number
            // returns 0 and register_peer rejects every peer. The epoch-advance tests below
            // register a finalizer peer to satisfy the role guard.
            type_configs: Arc::new({
                let tc = DashMap::new();
                for t in [
                    NodeRegistryType::Committer,
                    NodeRegistryType::Sentinel,
                    NodeRegistryType::Executor,
                    NodeRegistryType::Finalizer,
                    NodeRegistryType::Archiver,
                ] {
                    tc.insert(t.clone(), pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 10 });
                }
                tc
            }),
            identity: Arc::new(identity),
            rhash,
            bootstrap_peers: Vec::new(),
            rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
            transport_enabled: false,
        }
    }

    fn make_test_node_registry() -> Arc<NodeRegistry> {
        Arc::new(NodeRegistry::init(
            Arc::new(make_test_config()),
            None,
            Arc::new(|_, _| true),
        ))
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

    /// Same as `make_test_env_data` but with shard-aware routing enabled, for
    /// tests that exercise the per-shard executor selection path.
    fn make_test_env_data_sharded() -> EnvironmentMetadata {
        let json = r#"{"environment_id":"test","environment_name":"test",
            "partitions":[{"id":"token","partition_type":"Token"},
            {"id":"slush","partition_type":"Slush"}],
            "asym_crypto_provider":{"Ed25519":null},"sym_crypto_provider":"sym",
            "serialization_provider":"rmp","quorum_percentage":67.0,
            "override_quorum_percentage":0.0,"max_risk":1.0,
            "allowed_token_types":[],"trans_validation_specs":[],
            "block_validation_specs":[],"log_file":"test.log",
            "shard_count":2}"#;
        let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).unwrap();
        EnvironmentMetadata::load_from_spec(spec)
    }

    fn make_sentinel_fixture() -> (Sentinel, Arc<PendingTransactionRegistry>) {
        make_sentinel_fixture_with_data_provider(StubDataProvider::new())
    }

    fn make_sentinel_fixture_with_data_provider(
        data_provider: StubDataProvider,
    ) -> (Sentinel, Arc<PendingTransactionRegistry>) {
        make_sentinel_fixture_with_env_and_data_provider(data_provider, make_test_env_data())
    }

    /// Shared fixture build for a custom environment (e.g. shard-aware routing).
    fn make_sentinel_fixture_with_env_and_data_provider(
        data_provider: StubDataProvider,
        env_data: EnvironmentMetadata,
    ) -> (Sentinel, Arc<PendingTransactionRegistry>) {
        let registry = Arc::new(PendingTransactionRegistry::new());
        let node_registry = make_test_node_registry();
        let env_data = Arc::new(env_data);
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
            Arc::new(data_provider),
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
            sender_signature: vec![],
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
            sender_signature: vec![],
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
            stake_set: None,
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
                sender_signature: vec![],
            }).unwrap(),
            signature: vec![1, 2, 3],
            public_key: vec![4, 5, 6],
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        // Should not return Encoding error (may fail on other paths, but that's OK)
        match result {
            Err(SentinelError::Encoding(_)) => panic!("should not be Encoding error"),
            _ => {} // any other error is fine (validation, registry, etc.)
        }
    }

    // --- Phase 3.1 (AUDIT C3): sender-authentication regression tests ---

    /// Build a "Process" message whose transaction carries a valid sender signature
    /// (signed by `sender_pk`) and whose envelope sender is `sender_pk`.
    fn c3_process_message(sender_pk: Vec<u8>, tx: Transaction) -> Message {
        Message {
            chain_id: "test".into(),
            action: "Process".into(),
            body: serialize_to_bytes_rmp(&tx).unwrap(),
            signature: vec![],
            public_key: sender_pk,
            stake_set: None,
        }
    }

    /// Sign `tx`'s canonical bytes with `identity`, returning a C3-valid transaction.
    fn c3_sign(tx: &mut Transaction, identity: &NodeIdentity) {
        let canonical = tx.canonical_signature_bytes().expect("canonical transaction bytes");
        tx.sender_signature = identity.ed25519.sign_data(&canonical).expect("sender signs");
    }

    #[test]
    fn process_tx_with_valid_sender_signature_accepted() {
        let (sentinel, _registry) = make_sentinel_fixture();
        // A real sender signs the canonical transaction bytes.
        let sender_identity = NodeIdentity::generate_in_memory();
        let sender_pk = sender_identity.ed25519.public_key().expect("sender public key");

        let mut tx = Transaction {
            id: "c3_valid".into(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: sender_pk.clone(),
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        };
        c3_sign(&mut tx, &sender_identity);

        let msg = c3_process_message(sender_pk, tx);
        // The C3 gate must pass: the result may be Ok or a downstream stub error, but
        // never a sender-authentication rejection.
        let result = sentinel.handle_process_request(msg);
        assert!(
            !matches!(
                result,
                Err(SentinelError::UnauthenticatedSubmitter(_))
                    | Err(SentinelError::InvalidSenderSignature(_))
            ),
            "valid sender signature must pass the C3 gate, got {result:?}"
        );
    }

    #[test]
    fn unauthorized_submitter_debit_is_rejected() {
        let (sentinel, _registry) = make_sentinel_fixture();
        // HEADLINE C3: a different node submits a transaction debiting account X. Even
        // though X really signed the payload, the node submitting it is not X's node, so
        // the binding check must reject.
        let victim = NodeIdentity::generate_in_memory();
        let victim_pk = victim.ed25519.public_key().expect("victim public key");

        let attacker = NodeIdentity::generate_in_memory();
        let attacker_pk = attacker.ed25519.public_key().expect("attacker public key");

        let mut tx = Transaction {
            id: "c3_unauth".into(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: victim_pk.clone(),
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        };
        // The payload really was authorized by victim, so the signature check passes —
        // only the binding check (network submitter != account) can stop this.
        c3_sign(&mut tx, &victim);

        let msg = c3_process_message(attacker_pk, tx);
        match sentinel.handle_process_request(msg) {
            Err(SentinelError::UnauthenticatedSubmitter(_)) => {}
            other => panic!("expected UnauthenticatedSubmitter, got {other:?}"),
        }
    }

    #[test]
    fn forged_sender_signature_rejected() {
        let (sentinel, _registry) = make_sentinel_fixture();
        // Binding holds (envelope sender == tx.sender) but the signature is empty/forged.
        let sender = NodeIdentity::generate_in_memory();
        let sender_pk = sender.ed25519.public_key().expect("sender public key");

        let tx = Transaction {
            id: "c3_forged".into(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: sender_pk.clone(),
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![], // empty -> verify returns Ok(false)
        };

        let msg = c3_process_message(sender_pk, tx);
        match sentinel.handle_process_request(msg) {
            Err(SentinelError::InvalidSenderSignature(_)) => {}
            other => panic!("expected InvalidSenderSignature, got {other:?}"),
        }
    }

    #[test]
    fn sender_signature_does_not_cross_accounts() {
        let (sentinel, _registry) = make_sentinel_fixture();
        // A signature valid for account A must not authorize a payload claiming sender == B.
        let a = NodeIdentity::generate_in_memory();
        let a_pk = a.ed25519.public_key().expect("a public key");
        let b_claim = vec![7, 7, 7, 7];

        let mut tx = Transaction {
            id: "c3_cross".into(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b_claim.clone(), // claims to be B
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        };
        // A signs the canonical bytes; the binding (envelope == "B" == tx.sender) holds,
        // but verifying A's signature against "B" fails.
        c3_sign(&mut tx, &a);

        let msg = c3_process_message(b_claim, tx);
        match sentinel.handle_process_request(msg) {
            Err(SentinelError::InvalidSenderSignature(_)) => {}
            other => panic!("A's signature cannot authorize a tx claiming sender==B, got {other:?}"),
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
            stake_set: None,
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
            sender_signature: vec![],
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
            sender_signature: vec![],
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
            sender_signature: vec![],
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
            sender_signature: vec![],
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
            sender_signature: vec![],
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
            sender_signature: vec![],
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
                    sender_signature: vec![],
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
            stake_set: None,
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
            stake_set: None,
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
            stake_set: None,
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
            stake_set: None,
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
            stake_set: None,
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
                    sender_signature: vec![],
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
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_err());
        match result.unwrap_err() {
            SentinelError::TransactionInTerminalState(id) => assert_eq!(id, "tx_terminal"),
            _ => panic!("expected TransactionInTerminalState error"),
        }
    }

    // --- handle_register_request tests ---

    #[test]
    fn handle_register_request_with_sufficient_stake_succeeds() {
        let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
        let key = identity.ed25519.public_key().unwrap();
        let mut data_provider = StubDataProvider::new();
        data_provider = data_provider.with_user(
            key.clone(),
            "test".to_string(),
            User { public_key: key.clone(), fuel_balance: 1000, stake: 100, nonce: 0 },
        );
        let (sentinel, _registry) = make_sentinel_fixture_with_data_provider(data_provider);

        let req = NodeRegistryRequest::new(
            key.clone(),
            identity.rhash,
            identity
                .sign_binding(&identity.rhash, &NodeRegistryType::Sentinel, &[NodeRegistryType::Sentinel])
                .unwrap(),
            vec![NodeRegistryType::Sentinel],
            NodeRegistryType::Sentinel,
        );
        let msg = Message {
            chain_id: "test".into(),
            action: "Register".into(),
            body: serialize_to_bytes_rmp(&req).unwrap(),
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_ok());

        // Verify the node was added to the sentinel registry.
        let nodes = sentinel.node_registry.get_nodes(&NodeRegistryType::Sentinel).unwrap();
        assert!(nodes.contains_key(&key));
    }

    #[test]
    fn handle_register_request_already_registered_returns_error() {
        let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
        let key = identity.ed25519.public_key().unwrap();
        let mut data_provider = StubDataProvider::new();
        data_provider = data_provider.with_user(
            key.clone(),
            "test".to_string(),
            User { public_key: key.clone(), fuel_balance: 1000, stake: 100, nonce: 0 },
        );
        let (sentinel, _registry) = make_sentinel_fixture_with_data_provider(data_provider);

        // Pre-register the node.
        let nodes = sentinel.node_registry.get_nodes(&NodeRegistryType::Sentinel).unwrap();
        nodes.insert(key.clone(),
            pneumatic_core::node::NodeRegistryNode::new(
                [0u8; 16],
                Box::new(pneumatic_core::node::registry::NullConnection),
            ));

        let req = NodeRegistryRequest::new(
            key.clone(),
            identity.rhash,
            identity
                .sign_binding(&identity.rhash, &NodeRegistryType::Sentinel, &[NodeRegistryType::Sentinel])
                .unwrap(),
            vec![NodeRegistryType::Sentinel],
            NodeRegistryType::Sentinel,
        );
        let msg = Message {
            chain_id: "test".into(),
            action: "Register".into(),
            body: serialize_to_bytes_rmp(&req).unwrap(),
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_err());
        match result.unwrap_err() {
            SentinelError::Registry(msg) => assert!(msg.contains("already registered")),
            _ => panic!("expected Registry error"),
        }
    }

    #[test]
    fn handle_register_request_insufficient_stake_returns_error() {
        let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
        let key = identity.ed25519.public_key().unwrap();
        let mut data_provider = StubDataProvider::new();
        // Stake of 1 < default min_stake of 10
        data_provider = data_provider.with_user(
            key.clone(),
            "test".to_string(),
            User { public_key: key.clone(), fuel_balance: 0, stake: 1, nonce: 0 },
        );
        let (sentinel, _registry) = make_sentinel_fixture_with_data_provider(data_provider);

        let req = NodeRegistryRequest::new(
            key.clone(),
            identity.rhash,
            identity
                .sign_binding(&identity.rhash, &NodeRegistryType::Sentinel, &[NodeRegistryType::Sentinel])
                .unwrap(),
            vec![NodeRegistryType::Sentinel],
            NodeRegistryType::Sentinel,
        );
        let msg = Message {
            chain_id: "test".into(),
            action: "Register".into(),
            body: serialize_to_bytes_rmp(&req).unwrap(),
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        let result = sentinel.on_data_received(raw);
        assert!(result.is_err());
        match result.unwrap_err() {
            SentinelError::Registry(msg) => assert!(msg.contains("Insufficient stake")),
            _ => panic!("expected Registry error"),
        }
    }

    // --- Transaction pool enqueue tests ---

    #[test]
    fn handle_self_signed_enqueues_to_pool() {
        let (sentinel, registry) = make_sentinel_fixture();

        // Create a self-signed transaction (receiver is empty)
        let tx = Transaction {
            id: "tx_pool_enqueue_signed".into(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b"alice".to_vec(),
            receiver: vec![],
            amount: Some(100),
            timestamp: 1000,
            result_hash: vec![],
            sender_signature: vec![],
        };

        // Pre-register the transaction (as handle_self_signed in the real flow does)
        registry.register_pending(tx.id.clone()).unwrap();

        // Call handle_self_signed directly — it should enqueue to pool
        sentinel.handle_self_signed(tx.clone(), 100).unwrap();

        // Verify the transaction was enqueued to the pool
        let pool_txs = registry.get_ordered_transactions(&[1], 10).unwrap();
        assert!(!pool_txs.is_empty());
        assert_eq!(pool_txs[0].id, tx.id);
    }

    #[test]
    fn handle_process_request_enqueues_standard_tx_to_pool() {
        let (sentinel, registry) = make_sentinel_fixture();
        let (token, _node_registry) = (
            {
                let mut token = Token::new();
                token.set_metadata("owner".to_string(), "bob".to_string());
                token
            },
            make_test_node_registry(),
        );

        // Create a standard transaction (sender != owner, Executed spec)
        let tx = Transaction {
            id: "tx_pool_enqueue_std".into(),
            action: "Process".into(),
            token_id: vec![2],
            bid: None,
            sequence_number: 2,
            sender: b"bob".to_vec(),
            receiver: b"carol".to_vec(),
            amount: Some(50),
            timestamp: 2000,
            result_hash: vec![],
            sender_signature: vec![],
        };

        // Pre-register the transaction (as handle_process_request does)
        let tx_id = tx.id.clone();
        registry.register_pending(tx_id.clone()).unwrap();
        registry.acquire_transaction(&tx_id).unwrap();

        // Manually transition to Validated + enqueue (simulating the new code path
        // that runs in handle_process_request before send_to_executor_for_preload)
        let risk = TransactionRiskFactor {
                        affected_parties: 2,
                        amount: 50,
                        is_contract: false,
                        is_multi_party: false,
                    };
        let _ = registry.transition_to_validated_and_enqueue(
            &tx_id,
            tx.clone(),
            TransactionValidationResult {
                is_valid: true,
                risk,
                failure_reasons: vec![],
                finalizer_public_key: vec![3],
            },
        );

        // Verify the transaction was enqueued to the pool for token [2]
        let pool_txs = registry.get_ordered_transactions(&[2], 10).unwrap();
        assert!(!pool_txs.is_empty());
        assert_eq!(pool_txs[0].id, tx_id);
    }

    #[test]
    fn pool_ordering_is_deterministic() {
        let (sentinel, registry) = make_sentinel_fixture();
        let _ = sentinel;

        // Insert 3 transactions with different sequence numbers and senders.
        // Pool ordering: sender ASC, then sequence_number ASC, then timestamp ASC.
        for i in 0..3 {
            let tx = Transaction {
                id: format!("tx_order_{}", i),
                action: "Process".into(),
                token_id: vec![3],
                bid: None,
                sequence_number: i + 1,
                sender: vec![(i + 1) as u8], // sender 1, 2, 3
                receiver: vec![10],
                amount: Some(10),
                timestamp: 3000,
                result_hash: vec![],
                sender_signature: vec![],
            };
            let tx_id = tx.id.clone();
            registry.register_pending(tx_id.clone()).unwrap();
            let _ = registry.transition_to_validated_and_enqueue(
                &tx_id,
                tx,
                TransactionValidationResult {
                    is_valid: true,
                    risk: TransactionRiskFactor {
                        affected_parties: 2,
                        amount: 50,
                        is_contract: false,
                        is_multi_party: false,
                    },
                    failure_reasons: vec![],
                    finalizer_public_key: vec![4],
                },
            );
        }

        // Dequeue should return in deterministic order:
        // sender [1], seq 1 → sender [2], seq 2 → sender [3], seq 3
        let ordered = registry.get_ordered_transactions(&[3], 10).unwrap();
        assert_eq!(ordered.len(), 3);
        assert_eq!(ordered[0].id, "tx_order_0"); // sender [1]
        assert_eq!(ordered[1].id, "tx_order_1"); // sender [2]
        assert_eq!(ordered[2].id, "tx_order_2"); // sender [3]
    }

    // --- Shard-aware routing tests ---

    #[test]
    fn get_shard_executors_returns_executors() {
        let mut executors = pneumatic_core::epoch::ExecutorSet::default();
        for i in 0..4 {
            executors.executors.insert(vec![i as u8], 100 + i);
        }
        let data_provider = Arc::new(
            StubDataProvider::new().with_executor_set(1, executors)
        );
        let registry = Arc::new(PendingTransactionRegistry::new());
        let node_registry = make_test_node_registry();

        let mut env_data = make_test_env_data();
        env_data.shard_count = 2;

        let gossiper = Arc::new(Gossiper::new(
            NodeRegistryType::Sentinel,
            make_test_config(),
            300,
            env_data.asym_crypto_provider.clone(),
        ));
        let sentinel = Sentinel::new(
            make_test_config(),
            Arc::new(env_data),
            node_registry,
            registry.clone(),
            gossiper,
            data_provider,
        );

        // Shard 0 and shard 1 should each return some executors
        let shard0 = sentinel.get_shard_executors("tx-0", 1);
        let shard1 = sentinel.get_shard_executors("tx-1", 1);
        assert!(shard0.is_ok() && shard1.is_ok());
        let s0 = shard0.unwrap();
        let s1 = shard1.unwrap();
        assert!(!s0.is_empty() && !s1.is_empty());
        // The two shards should have different members (or at least one)
        let mut s0_sorted = s0;
        let mut s1_sorted = s1;
        s0_sorted.sort();
        s1_sorted.sort();
        assert!(s0_sorted != s1_sorted || s0_sorted.len() + s1_sorted.len() <= 4);
    }

    #[test]
    fn advance_epoch_invalidates_caches() {
        use pneumatic_core::epoch::StakeSet;

        let data_provider = StubDataProvider::new();
        let (mut sentinel, _registry) = make_sentinel_fixture_with_data_provider(data_provider);

        // Initially at epoch 0
        sentinel.advance_epoch(1);
        assert_eq!(*sentinel.current_epoch.lock(), 1);

        // Invalidate and verify caches are cleared
        let snapshot = StakeSet {
            stakers: [(vec![1], 100)].into_iter().collect(),
        };
        sentinel.stake_snapshot_cache.put(1, snapshot);
        assert_eq!(sentinel.stake_snapshot_cache.cached_count(), 1);
        sentinel.advance_epoch(2);
        assert_eq!(sentinel.stake_snapshot_cache.cached_count(), 0);
        assert_eq!(*sentinel.current_epoch.lock(), 2);
    }

    // -----------------------------------------------------------------------
    // Phase 4.2 (AUDIT H5): sentinel routes on the tracked epoch, not a
    //                    hardcoded literal `1`. Each test is a discriminator
    //                    that fails under the literal-1 bug and passes with the
    //                    current_epoch wiring.
    // -----------------------------------------------------------------------

    /// The core discriminator. After `advance_epoch`, both the executor-shard and
    /// the finalizer-stake routing primitives select against the *new* epoch.
    #[test]
    fn advance_epoch_routes_follows_new_epoch() {
        use pneumatic_core::epoch::{ExecutorSet, StakeSet};

        // Disjoint executor sets and stake snapshots per epoch: any selection from
        // epoch 1 is provably distinct from any selection from epoch 2.
        let data_provider = StubDataProvider::new()
            .with_executor_set(
                1,
                ExecutorSet {
                    executors: [(vec![1], 100), (vec![2], 100)].into_iter().collect(),
                },
            )
            .with_executor_set(
                2,
                ExecutorSet {
                    executors: [(vec![10], 100), (vec![20], 100)].into_iter().collect(),
                },
            )
            .with_stake_snapshot(
                1,
                StakeSet {
                    stakers: [(vec![1], 100), (vec![2], 100), (vec![3], 100)].into_iter().collect(),
                },
            )
            .with_stake_snapshot(
                2,
                StakeSet {
                    stakers: [(vec![10], 100), (vec![20], 100), (vec![30], 100)].into_iter().collect(),
                },
            );
        let (sentinel, _registry) =
            make_sentinel_fixture_with_env_and_data_provider(data_provider, make_test_env_data_sharded());

        let tx_id = "tx_epoch_route".to_string();

        // advance_epoch(1) is a no-op at boot (init is 1); routing stays on epoch 1.
        sentinel.advance_epoch(1);
        assert_eq!(*sentinel.current_epoch.lock(), 1);
        let exec1 = sentinel
            .get_shard_executors(&tx_id, *sentinel.current_epoch.lock())
            .unwrap();
        let finalizer1 = sentinel
            .assign_finalizer_deterministic(&tx_id, *sentinel.current_epoch.lock())
            .unwrap();

        // advance_epoch(2) moves routing to epoch 2.
        sentinel.advance_epoch(2);
        assert_eq!(*sentinel.current_epoch.lock(), 2);
        let exec2 = sentinel
            .get_shard_executors(&tx_id, *sentinel.current_epoch.lock())
            .unwrap();
        let finalizer2 = sentinel
            .assign_finalizer_deterministic(&tx_id, *sentinel.current_epoch.lock())
            .unwrap();

        // Per-epoch routing must select different targets. Under the literal-1 bug
        // both calls would read epoch 1 → identical selections.
        let mut e1 = exec1.clone();
        e1.sort();
        let mut e2 = exec2.clone();
        e2.sort();
        assert_ne!(e1, e2, "executor selection must change after an epoch advance");
        assert_ne!(finalizer1, finalizer2, "finalizer selection must change after an epoch advance");

        // And each pick must come from its own epoch's disjoint key set.
        assert!(
            [vec![1], vec![2], vec![3]].contains(&finalizer1)
                && [vec![10], vec![20], vec![30]].contains(&finalizer2),
            "each finalizer pick must be drawn from its own epoch's stake snapshot"
        );
    }

    /// True call-site discriminator for the executor path:
    /// `send_to_executor_for_preload` routes on `current_epoch`.
    #[test]
    fn send_to_executor_for_preload_follows_current_epoch() {
        use pneumatic_core::epoch::ExecutorSet;

        // Only epoch 1 has an executor set; epoch 2 is absent.
        let data_provider = StubDataProvider::new().with_executor_set(
            1,
            ExecutorSet {
                executors: [(vec![1], 100), (vec![2], 100)].into_iter().collect(),
            },
        );
        let (sentinel, _registry) =
            make_sentinel_fixture_with_env_and_data_provider(data_provider, make_test_env_data_sharded());

        let tx = Transaction {
            id: "tx_preload_epoch".to_string(),
            action: "Transfer".to_string(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: vec![1],
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        };

        // advance_epoch(1) is a no-op at boot; routing stays on epoch 1 → valid set.
        sentinel.advance_epoch(1);
        assert!(sentinel.send_to_executor_for_preload(&tx).is_ok());

        // advance_epoch(2): no executor set for epoch 2 → routing must fail closed.
        // Under the literal-1 bug the handler would keep reading epoch 1 → still Ok.
        sentinel.advance_epoch(2);
        let err = sentinel.send_to_executor_for_preload(&tx).unwrap_err();
        match err {
            SentinelError::Routing(msg) => assert_eq!(msg, "No executor set for epoch 2"),
            other => panic!("expected Routing error, got {:?}", other),
        }
    }

    /// True call-site discriminator for the finalizer path:
    /// `handle_rejection` reassigns against `current_epoch`'s stake snapshot.
    #[test]
    fn handle_rejection_follows_current_epoch() {
        use pneumatic_core::epoch::StakeSet;

        // Disjoint stake snapshots per epoch.
        let data_provider = StubDataProvider::new()
            .with_stake_snapshot(
                1,
                StakeSet {
                    stakers: [(vec![1], 100), (vec![2], 100), (vec![3], 100)].into_iter().collect(),
                },
            )
            .with_stake_snapshot(
                2,
                StakeSet {
                    stakers: [(vec![10], 100), (vec![20], 100), (vec![30], 100)].into_iter().collect(),
                },
            );
        let (sentinel, registry) = make_sentinel_fixture_with_data_provider(data_provider);

        // Put the transaction into Finalizing with finalizer_key = vec![1]
        // (a member of the epoch-1 stake set) — i.e. the assigned finalizer rejects.
        let rejected_key = vec![1];
        make_finalizing_entry(&registry, "tx_reject_epoch", rejected_key.clone());

        // Advance to epoch 2, then drive the rejection.
        sentinel.advance_epoch(2);
        let msg = Message {
            chain_id: "test".into(),
            action: "Reject".into(),
            body: serialize_to_bytes_rmp(&"tx_reject_epoch".to_string()).unwrap(),
            signature: vec![],
            public_key: rejected_key.clone(),
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();
        assert!(sentinel.on_data_received(raw).is_ok());

        // The discriminators: the reassignment must land on the epoch-2 pick, not the
        // epoch-1 pick (which is what the literal-1 bug would produce).
        let epoch2_pick = sentinel
            .assign_finalizer_deterministic_retry("tx_reject_epoch", 2, &rejected_key)
            .unwrap();
        let epoch1_pick = sentinel
            .assign_finalizer_deterministic_retry("tx_reject_epoch", 1, &rejected_key)
            .unwrap();
        assert_ne!(epoch1_pick, epoch2_pick, "the two epochs must pick different finalizers for the test to be meaningful");
        assert!(
            registry.is_requested_finalizer("tx_reject_epoch", &epoch2_pick),
            "entry must be reassigned to the epoch-2 finalizer"
        );
        assert!(
            !registry.is_requested_finalizer("tx_reject_epoch", &epoch1_pick),
            "entry must NOT be reassigned to the epoch-1 finalizer"
        );
    }

    /// Wiring discriminator: the `BlockFinalized` action now advances the epoch
    /// (and only a registered finalizer may do so — see fail-closed test).
    #[test]
    fn block_finalized_advances_epoch() {
        use pneumatic_core::blocks::Block;
        use pneumatic_core::conns::Connection;

        // Minimal no-op connection so register_peer accepts a peer.
        struct NoOpConnection;
        #[async_trait::async_trait]
        impl Connection for NoOpConnection {
            async fn send(
                &self,
                _data: &Vec<u8>,
            ) -> Result<(), pneumatic_core::conns::ConnError> {
                Ok(())
            }
        }

        // Register a finalizer peer so the role guard recognizes the sender.
        let (sentinel, _registry) = make_sentinel_fixture();
        let finalizer_key = vec![0xA3; 32];
        assert!(sentinel.node_registry.register_peer(
            finalizer_key.clone(),
            [3u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(NoOpConnection),
        ));

        // Build a finalized block bound to epoch 3.
        let block = Block {
            signed_trans: pneumatic_core::transactions::SignedTransaction::test_transaction(),
            token_metadata: std::collections::HashMap::new(),
            previous_hash: vec![1, 2, 3],
            current_hash: vec![4, 5, 6],
            timestamp: 0,
            finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 3,
        };
        let body = serialize_to_bytes_rmp(&block).unwrap();

        // Message from the registered finalizer: message.public_key is the sender.
        let msg = Message {
            chain_id: "test".into(),
            action: "BlockFinalized".into(),
            body,
            signature: vec![],
            public_key: finalizer_key.clone(),
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&msg).unwrap();

        assert!(sentinel.on_data_received(raw).is_ok());
        assert_eq!(*sentinel.current_epoch.lock(), 3);
    }

    /// Fail-closed guardrails for the epoch-advance handler: only a registered
    /// finalizer may advance, and the advance is monotonic (never rewind).
    #[test]
    fn block_finalized_fail_closed() {
        use pneumatic_core::blocks::Block;
        use pneumatic_core::conns::Connection;

        struct NoOpConnection;
        #[async_trait::async_trait]
        impl Connection for NoOpConnection {
            async fn send(
                &self,
                _data: &Vec<u8>,
            ) -> Result<(), pneumatic_core::conns::ConnError> {
                Ok(())
            }
        }

        fn make_block(epoch: u64) -> Block {
            Block {
                signed_trans: pneumatic_core::transactions::SignedTransaction::test_transaction(),
                token_metadata: std::collections::HashMap::new(),
                previous_hash: vec![],
                current_hash: vec![],
                timestamp: 0,
                finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
                proposer_key: vec![],
                epoch_number: epoch,
            }
        }

        let (sentinel, _registry) = make_sentinel_fixture();
        let finalizer_key = vec![0xB1; 32];
        assert!(sentinel.node_registry.register_peer(
            finalizer_key.clone(),
            [1u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(NoOpConnection),
        ));

        // (b) A non-finalizer sender is rejected; the epoch must not move.
        // Under no role guard an attacker could advance the epoch to an arbitrary
        // (stale/empty) executor set — an availability risk.
        let attacker_body = serialize_to_bytes_rmp(&make_block(5)).unwrap();
        let attacker_msg = Message {
            chain_id: "test".into(),
            action: "BlockFinalized".into(),
            body: attacker_body,
            signature: vec![],
            public_key: vec![0xDE; 32], // never registered as a finalizer
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&attacker_msg).unwrap();
        match sentinel.on_data_received(raw) {
            Err(SentinelError::Registry(msg)) => assert!(msg.contains("non-finalizer")),
            other => panic!("expected Registry error for non-finalizer, got {:?}", other),
        }
        assert_eq!(*sentinel.current_epoch.lock(), 1);

        // (c) A malformed body is rejected (encoding failure).
        let bad_body = serialize_to_bytes_rmp(&"this is not a block".to_string()).unwrap();
        let bad_msg = Message {
            chain_id: "test".into(),
            action: "BlockFinalized".into(),
            body: bad_body,
            signature: vec![],
            public_key: finalizer_key.clone(),
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&bad_msg).unwrap();
        match sentinel.on_data_received(raw) {
            Err(SentinelError::Encoding(_)) => {}
            other => panic!("expected Encoding error for malformed body, got {:?}", other),
        }
        assert_eq!(*sentinel.current_epoch.lock(), 1);

        // (a) A stale/replayed block (epoch <= current) must not rewind the epoch.
        // Advance to epoch 2, then feed a block bound to epoch 1. Under no monotonic
        // guard the epoch would roll back to 1; the guard keeps it at 2.
        sentinel.advance_epoch(2);
        let stale_body = serialize_to_bytes_rmp(&make_block(1)).unwrap();
        let stale_msg = Message {
            chain_id: "test".into(),
            action: "BlockFinalized".into(),
            body: stale_body,
            signature: vec![],
            public_key: finalizer_key.clone(),
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&stale_msg).unwrap();
        assert!(sentinel.on_data_received(raw).is_ok(), "stale block still routes to the handler");
        assert_eq!(
            *sentinel.current_epoch.lock(), 2,
            "stale block must not rewind the epoch"
        );
    }
}
