use std::collections::HashMap;
use std::sync::Arc;

use ed25519_dalek::{SigningKey, VerifyingKey};
use tokio::sync::Mutex;

use pneumatic_core::blocks::Block;
use pneumatic_core::config::Config;
use pneumatic_core::crypto::HashProvider;
use pneumatic_core::data::DataProvider;
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::errors::{PneumaticError, ReconciledSignatures};
use pneumatic_core::epoch::StakeSet;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::registry::{PendingTransactionRegistry, TransactionSignatureRegistry};
use pneumatic_core::transactions::{
    PendingTransaction, SignedTransaction, Transaction, TransactionCommit, TransactionSignature,
    TransactionState, TransactionValidationResult,
};

use crate::block_builder::BlockBuilder;
use crate::message_dispatcher::MessageDispatcher;
use crate::signature_collector::SignatureCollector;
use crate::stake_snapshot_cache::StakeSnapshotCache;

/// Convert a byte slice to a hex string (lowercase, no prefix).
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// ---------------------------------------------------------------------------
// Finalizer — quorum checking, block formation, and distribution
// ---------------------------------------------------------------------------

/// The Finalizer orchestrates the quorum-checking and block-building pipeline.
/// It is the split counterpart to the C# TransactionReconciler, decomposed into
/// three focused components:
///
/// - **SignatureCollector**: Collects and verifies executor signatures, checks quorum
/// - **BlockBuilder**: Builds SignedTransaction and Block from reconciled signatures
/// - **MessageDispatcher**: Sends blocks to Committers, clears to Sentinels
///
/// Flow:
/// 1. Preload: Executor sends transaction data → handle_preload
/// 2. Sign: Executors send execution signatures → handle_signature
/// 3. Quorum met → try_finalize (reconcile → build block → dispatch)
/// 4. Clear: Send clear notification to Sentinels
pub struct Finalizer {
    /// Environment ID
    env_id: String,
    /// Public key of this finalizer node
    public_key: Vec<u8>,
    /// Shared registry of connected nodes
    node_registry: Arc<NodeRegistry>,
    /// Transaction registry for state tracking
    pending_registry: Arc<PendingTransactionRegistry>,
    /// Signature collection registry
    signature_registry: Arc<TransactionSignatureRegistry>,
    /// Signature collector component
    signature_collector: SignatureCollector,
    /// Block builder component
    block_builder: BlockBuilder,
    /// Message dispatcher component
    message_dispatcher: MessageDispatcher,
    /// In-flight preload tasks keyed by transaction ID
    preload_tasks: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// Flag: is the finalizer shutting down?
    awaiting_shutdown: Arc<Mutex<bool>>,
    /// Current epoch number — used when building blocks.
    /// Updated when blocks from the chain are received.
    current_epoch: u64,
    /// Stake snapshot cache — fetches the current epoch's stake set from the
    /// DataProvider (with local caching) for quorum gossip.
    stake_cache: StakeSnapshotCache,
    /// Current stake set for quorum gossip.
    /// Manual override via `set_stake_set` — used by tests. In production this
    /// stays `None` and `get_stake_set_for_epoch` uses the cache instead.
    stake_set: Option<StakeSet>,
    /// DataProvider — used to look up a token's chain tip when building
    /// blocks (`resolve_previous_hash`).
    data_provider: Arc<dyn DataProvider>,
    /// Token partition ID — DataProvider key for token lookups.
    partition_id: String,
}

impl Finalizer {
    /// Create a new Finalizer with all required components.
    ///
    /// `quorum_percentage` is the threshold for quorum (e.g., 67.0 for 2/3).
    /// `total_voters` is the total number of voting nodes.
    /// `signing_key` is the Ed25519 private key for signing blocks.
    /// `verifying_key` is derived from the signing key.
    /// `leader_address/stake/hash` are from the environment's leader.
    /// `current_epoch` is the starting epoch number for block building.
    /// `data_provider` is used to fetch the current epoch's stake snapshot
    /// for quorum gossip (`BlockFinalized` messages) and a token's chain tip
    /// for block `previous_hash` resolution.
    /// `partition_id` is the token partition ID used as the DataProvider key.
    pub fn new(
        env_id: String,
        public_key: Vec<u8>,
        node_registry: Arc<NodeRegistry>,
        pending_registry: Arc<PendingTransactionRegistry>,
        signature_registry: Arc<TransactionSignatureRegistry>,
        quorum_percentage: f32,
        total_voters: u32,
        signing_key: SigningKey,
        verifying_key: VerifyingKey,
        hash_provider: Arc<dyn HashProvider>,
        leader_address: Vec<u8>,
        leader_stake: u64,
        leader_hash: Vec<u8>,
        current_epoch: u64,
        data_provider: Arc<dyn DataProvider>,
        partition_id: String,
    ) -> Self {
        let signature_collector = SignatureCollector::new(
            signature_registry.clone(),
            quorum_percentage,
            total_voters,
        );

        let finalizer_addr = verifying_key.to_bytes().to_vec();
        let message_dispatcher = MessageDispatcher::new(
            node_registry.clone(),
            env_id.clone(),
            public_key.clone(),
            vec![], // Signature set in initialize
        );

        let block_builder = BlockBuilder::new(
            signing_key,
            verifying_key,
            hash_provider,
            leader_address,
            leader_stake,
            leader_hash,
            finalizer_addr,
        );

        Finalizer {
            env_id,
            public_key,
            node_registry,
            pending_registry,
            signature_registry,
            signature_collector,
            block_builder,
            message_dispatcher,
            preload_tasks: Arc::new(Mutex::new(HashMap::new())),
            awaiting_shutdown: Arc::new(Mutex::new(false)),
            current_epoch,
            stake_cache: StakeSnapshotCache::new(data_provider.clone(), partition_id.clone()),
            stake_set: None,
            data_provider,
            partition_id,
        }
    }

    /// Set the stake set for this finalizer.
    ///
    /// This is used for quorum gossip: the finalizer includes the stake set
    /// in `BlockFinalized` messages so receiving nodes can perform
    /// stake-weighted confirmation tracking.
    pub fn set_stake_set(&mut self, stake_set: StakeSet) {
        self.stake_set = Some(stake_set);
    }

    /// Get the current stake set, if set manually.
    pub fn get_stake_set(&self) -> Option<&StakeSet> {
        self.stake_set.as_ref()
    }

    /// Get the stake set for the current epoch.
    ///
    /// Priority: (1) manually set via `set_stake_set` (test override),
    /// (2) cached/fetched from DataProvider (production path).
    fn get_stake_set_for_epoch(&self) -> Option<StakeSet> {
        if let Some(s) = &self.stake_set {
            return Some(s.clone());
        }
        self.stake_cache.get(self.current_epoch)
    }

    /// Resolve the chain tip (`previous_hash`) for the token a transaction targets.
    ///
    /// Reads `last_hash_in` from the token's current chain state. On an empty
    /// chain (or an invalid one — `ChainState::invalid()` also yields `vec![]`)
    /// this is `vec![]`, which is the correct genesis prev-hash for the
    /// committer's strict linkage check. On lookup failure we fall back to
    /// `vec![]` rather than failing finalization: a stale/empty prev-hash is
    /// dropped non-fatally by committers anyway, while a hard error would
    /// permanently stall the transaction (signatures are already collected).
    ///
    /// Note: `get_token` clones the full token (chain length is bounded by the
    /// token's `security_level`), which is acceptable at once-per-finalize.
    fn resolve_previous_hash(&self, token_id: &Vec<u8>) -> Vec<u8> {
        match self.data_provider.get_token(token_id, &self.partition_id) {
            Ok(token) => token.blockchain.get_current_chain_state().last_hash_in,
            Err(e) => {
                log::warn!(
                    "resolve_previous_hash: failed to load token for previous_hash lookup ({}): {}; falling back to empty prev-hash",
                    bytes_to_hex(token_id),
                    e
                );
                vec![]
            }
        }
    }

    /// Resolve `(total_stake, total_voters)` for the standard finalize path
    /// from the current epoch's stake set (manual override or DataProvider
    /// cache). Falls back to `(0, 0)` when no stake set is available.
    fn resolve_stake_metrics(&self) -> (u64, u32) {
        match self.get_stake_set_for_epoch() {
            Some(set) => (set.total_stake(), set.stakers.len() as u32),
            None => (0, 0),
        }
    }

    /// Initialize the finalizer — subscribe to message handlers.
    ///
    /// This method would normally set up the gossiper to receive messages
    /// with actions "Preload" and "Sign". Currently a stub — the closure
    /// parameter represents the message handler.
    pub fn initialize<F>(&self, _on_message_received: F)
    where
        F: Fn(Message) + Send + Sync + 'static,
    {
        // In production: subscribe to "Preload" and "Sign" actions
        // via the Gossiper message router.
        // This requires injecting a Gossiper into the Finalizer struct.
        // For now, the closure is accepted but not wired.
        let _ = _on_message_received;
    }

    /// Handle a Preload message from the Sentinel/Executor.
    ///
    /// Receives preloaded transaction data and stores it for later processing.
    /// Returns an acknowledgement message.
    pub async fn handle_preload(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
        // Deserialize the transaction payload
        let tx: Transaction = deserialize_rmp_to(&message.body)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Store as a preload task
        self.preload_tasks.lock().await.insert(tx.id.clone(), message.signature.clone());

        // Acknowledge receipt
        Ok(pneumatic_core::messages::acknowledge())
    }

    /// Handle a Signature message from an Executor.
    ///
    /// OPTIMISTIC: First valid signature triggers immediate optimistic finalize.
    /// Subsequent signatures accumulate stake and are acknowledged.
    /// If quorum is eventually reached, the transaction is upgraded to confirmed.
    ///
    /// Returns an acknowledgement if added, or the result of optimistic finalize
    /// if this signature completed the optimistic path.
    pub async fn handle_signature(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
        // Deserialize the executor signature
        let sig: TransactionSignature = deserialize_rmp_to(&message.body)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Extract transaction ID from the signature
        let tx_id = String::from_utf8_lossy(&sig.transaction_id).to_string();

        // Extract executor public key
        let executor_key = message.public_key.clone();

        // Add the signature to the collector
        self.signature_collector
            .add_signature(&tx_id, executor_key.clone(), sig.clone())?;

        // OPTIMISTIC: First valid signature → try optimistic finalize immediately
        if self.signature_collector.signature_count(&tx_id) == 1 {
            return self.try_finalize_optimistic(&tx_id, &sig, &executor_key).await;
        }

        // Subsequent signatures — just acknowledge (stake accumulates in background)
        // If quorum is eventually reached, the transaction will be confirmed
        Ok(pneumatic_core::messages::acknowledge())
    }

    /// Attempt to finalize a transaction after quorum is reached.
    ///
    /// This is the core pipeline step:
    /// 1. Reconcile executor signatures
    /// 2. Build the SignedTransaction
    /// 3. Sign the finalizer's portion
    /// 4. Build the Block
    /// 5. Send to Committers
    /// 6. Clear Sentinels
    async fn try_finalize(&self, tx_id: &str) -> Result<Vec<u8>, PneumaticError> {
        // Step 1: Reconcile collected signatures
        let reconciled = self.signature_collector.reconcile_signatures(tx_id)?;

        // Step 2: Load the transaction from pending registry
        let entry = self.pending_registry.get_transaction_mut(tx_id)?;
        let transaction = match entry.state {
            TransactionState::Preloaded { ref transaction }
            | TransactionState::Validated { ref transaction, .. }
            | TransactionState::Executing { ref transaction } => {
                transaction.clone()
            }
            _ => {
                return Err(PneumaticError::Registry(format!(
                    "Transaction {} not in executable state for finalization",
                    tx_id
                )));
            }
        };
        drop(entry);

        // Step 3: Get the finalizer key from the transaction state
        let finalizer_key = match self.pending_registry.get_transaction_mut(tx_id) {
            Ok(entry) => match &entry.state {
                TransactionState::Validated { validation, .. } => {
                    validation.finalizer_public_key.clone()
                }
                TransactionState::Finalizing { finalizer_key, .. } => {
                    finalizer_key.clone()
                }
                _ => vec![],
            },
            Err(_) => vec![],
        };

        // Step 4: Build SignedTransaction from reconciled data, using the
        // current epoch's stake set for the voter metrics.
        let (total_stake, total_voters) = self.resolve_stake_metrics();

        let mut signed_tx = self.block_builder.build_signed_transaction(
            &reconciled,
            &transaction,
            total_stake,
            total_voters,
        );

        // Step 5: Sign the finalizer's portion
        let finalizer_sig = self.block_builder.sign_finalizer_block(&mut signed_tx).await?;

        signed_tx.finalizer_sig = finalizer_sig;

        // Step 6: Transition to Finalizing state
        if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
            entry.transition_to_finalizing(transaction.clone(), finalizer_key);
        }

        // Step 7: Create the Block, chained to the token's current chain tip.
        let previous_hash = self.resolve_previous_hash(&transaction.token_id);
        let block = self.block_builder.create_block(signed_tx.clone(), previous_hash, self.current_epoch);

        // Step 8: Send the commit to all Committers
        let block_hash = block.current_hash.clone();
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: transaction.token_id.clone(),
            env_id: self.env_id.clone(),
            proposed_block: block,
        };
        self.message_dispatcher.send_to_committers(commit).await?;

        // Step 9: Send clear to all Sentinels
        self.message_dispatcher.send_clear_to_sentinels(tx_id).await?;

        // Step 10: Transition to Committed state
        if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
            entry.transition_to_committed(transaction, block_hash);
        }

        // Clean up preload tasks
        self.preload_tasks.lock().await.remove(tx_id);

        // Clean up signature registry
        let _ = self.signature_registry.try_remove_transaction(tx_id);

        // Acknowledge success
        Ok(serialize_to_bytes_rmp(&tx_id.as_bytes().to_vec())
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?)
    }

    /// Attempt to finalize a transaction optimistically (first executor signature).
    ///
    /// This is the fast path — no quorum waiting, no signature reconciliation.
    /// The single executor's honest signature is proof enough for optimistic commit.
    /// Subsequent signatures accumulate stake in the background.
    async fn try_finalize_optimistic(
        &self,
        tx_id: &str,
        single_sig: &TransactionSignature,
        executor_key: &[u8],
    ) -> Result<Vec<u8>, PneumaticError> {
        // Step 1: Load the transaction from pending registry
        let entry = self.pending_registry.get_transaction_mut(tx_id)?;
        let transaction = match entry.state {
            TransactionState::Preloaded { ref transaction }
            | TransactionState::Validated { ref transaction, .. }
            | TransactionState::Executing { ref transaction } => {
                transaction.clone()
            }
            _ => {
                return Err(PneumaticError::Registry(format!(
                    "Transaction {} not in executable state for finalization",
                    tx_id
                )));
            }
        };
        drop(entry);

        // Step 2: Get the finalizer key from the transaction state
        let finalizer_key = match self.pending_registry.get_transaction_mut(tx_id) {
            Ok(entry) => match &entry.state {
                TransactionState::Validated { validation, .. } => {
                    validation.finalizer_public_key.clone()
                }
                TransactionState::Finalizing { finalizer_key, .. } => {
                    finalizer_key.clone()
                }
                _ => vec![],
            },
            Err(_) => vec![],
        };

        // Step 3: Build SignedTransaction using the single executor's signature
        let mut signed_tx = self.block_builder.build_signed_transaction_optimistic(
            single_sig,
            &transaction,
            executor_key,
        );

        // Step 4: Sign the finalizer's portion
        let finalizer_sig = self.block_builder.sign_finalizer_block(&mut signed_tx).await?;
        signed_tx.finalizer_sig = finalizer_sig;

        // Step 5: Transition to Finalizing state
        if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
            entry.transition_to_finalizing(transaction.clone(), finalizer_key);
        }

        // Step 6: Create the Block with optimistic finality, chained to the
        // token's current chain tip.
        let previous_hash = self.resolve_previous_hash(&transaction.token_id);
        let block = self.block_builder.create_block_optimistic(
            signed_tx.clone(),
            previous_hash,
            self.current_epoch,
        );

        // Step 7: Send the commit to all Committers
        let block_hash = block.current_hash.clone();
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: transaction.token_id.clone(),
            env_id: self.env_id.clone(),
            proposed_block: block.clone(),
        };
        self.message_dispatcher.send_to_committers(commit).await?;

        // Step 7.5: Broadcast block finalized to all committers and archivars via gossip.
        // Fetches the current epoch's stake set (cached) so receiving nodes
        // can perform stake-weighted confirmation tracking.
        let stake_set = self.get_stake_set_for_epoch();
        self.message_dispatcher
            .send_block_finalized(block, stake_set)
            .await?;

        // Step 8: Send clear to all Sentinels
        self.message_dispatcher.send_clear_to_sentinels(tx_id).await?;

        // Step 9: Transition to Committed state
        if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
            entry.transition_to_committed(transaction, block_hash);
        }

        // Clean up preload tasks
        self.preload_tasks.lock().await.remove(tx_id);

        // Clean up signature registry
        let _ = self.signature_registry.try_remove_transaction(tx_id);

        // Acknowledge success
        Ok(serialize_to_bytes_rmp(&tx_id.as_bytes().to_vec())
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?)
    }

    /// Check if the finalizer is shutting down.
    pub async fn is_shutting_down(&self) -> bool {
        *self.awaiting_shutdown.lock().await
    }

    /// Set the finalizer shutdown flag.
    pub async fn initiate_shutdown(&self) {
        *self.awaiting_shutdown.lock().await = true;
    }

    /// Get the number of in-flight preload tasks.
    pub async fn preload_task_count(&self) -> usize {
        self.preload_tasks.lock().await.len()
    }

    /// Get the number of collected signatures for a transaction.
    pub fn signature_count(&self, tx_id: &str) -> usize {
        self.signature_collector.signature_count(tx_id)
    }

    /// Get the finalizer's current epoch number.
    pub fn current_epoch(&self) -> u64 {
        self.current_epoch
    }

    /// Advance to a new epoch. Called when the finalizer receives
    /// blocks indicating an epoch transition.
    ///
    /// Invalidates the stake snapshot cache so the new epoch's stake set
    /// is freshly fetched from the DataProvider on next use.
    pub fn advance_epoch(&mut self) {
        self.current_epoch += 1;
        self.stake_cache.invalidate_all();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use pneumatic_core::config::Config;
    use pneumatic_core::crypto::BasicHashProvider;
    use pneumatic_core::data::StubDataProvider;
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::blocks::{BlockFactory, FinalityStatus};
    use pneumatic_core::node::{NodeRegistryType, NodeType};
    use pneumatic_core::tokens::Token;
    use pneumatic_core::transactions::{PendingTransaction, TransactionState};
    use rand::RngCore;

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
            node_registry_types: vec![NodeRegistryType::Finalizer],
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
            result_hash: vec![1, 2, 3, 4],
        };
        let validation = TransactionValidationResult::valid(
            vec![5, 6, 7, 8], // finalizer key
            pneumatic_core::errors::TransactionRiskFactor {
                affected_parties: 2,
                amount: 100,
                is_contract: false,
                is_multi_party: false,
            },
        );
        let pending = PendingTransaction::new("test_tx_001".to_string(), TransactionState::Validated {
            transaction: tx,
            validation,
        });
        let _ = registry.add_transaction("test_tx_001".to_string(), pending);
        registry
    }

    fn make_test_signing_key() -> (SigningKey, VerifyingKey) {
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        (signing_key, verifying_key)
    }

    fn make_finalizer(
        pending_registry: Arc<PendingTransactionRegistry>,
    ) -> Finalizer {
        let node_registry = make_test_node_registry();
        let signature_registry = Arc::new(TransactionSignatureRegistry::new());
        let hash_provider = Arc::new(BasicHashProvider::new());

        let (signing_key, verifying_key) = make_test_signing_key();

        Finalizer::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            pending_registry,
            signature_registry,
            67.0,   // quorum
            3,      // total voters
            signing_key,
            verifying_key,
            hash_provider,
            vec![10, 20, 30], // leader address
            100,              // leader stake
            vec![40, 50, 60], // leader hash
            0,                // current_epoch
            Arc::new(StubDataProvider::new()),
            "test_env".to_string(),
        )
    }

    /// Factory that wires a DataProvider with a pre-seeded stake snapshot
    /// for the given epoch, so stake fetching tests can verify the
    /// `BlockFinalized` gossip path.
    fn make_finalizer_with_data_provider(
        pending_registry: Arc<PendingTransactionRegistry>,
        data_provider: Arc<StubDataProvider>,
    ) -> Finalizer {
        let node_registry = make_test_node_registry();
        let signature_registry = Arc::new(TransactionSignatureRegistry::new());
        let hash_provider = Arc::new(BasicHashProvider::new());

        let (signing_key, verifying_key) = make_test_signing_key();

        Finalizer::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            node_registry,
            pending_registry,
            signature_registry,
            67.0,   // quorum
            3,      // total voters
            signing_key,
            verifying_key,
            hash_provider,
            vec![10, 20, 30], // leader address
            100,              // leader stake
            vec![40, 50, 60], // leader hash
            0,                // current_epoch
            data_provider,
            "test_env".to_string(),
        )
    }

    #[test]
    fn test_finalizer_creation() {
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer(pending_registry);

        assert_eq!(finalizer.env_id, "test_env");
        assert_eq!(finalizer.public_key, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_handle_preload() {
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer(pending_registry);

        let tx = Transaction {
            id: "preload_tx".to_string(),
            action: "Transfer".to_string(),
            token_id: vec![0, 1, 2],
            bid: None,
            sequence_number: 1,
            sender: vec![],
            receiver: vec![],
            amount: Some(50),
            timestamp: 1000,
            result_hash: vec![],
        };
        let body = serialize_to_bytes_rmp(&tx).unwrap();
        let message = Message {
            chain_id: "test_env".to_string(),
            action: String::from("Preload"),
            body,
            signature: vec![],
            public_key: vec![9, 8, 7],
            stake_set: None,
        };

        let result = finalizer.handle_preload(&message).await;
        assert!(result.is_ok());
        assert_eq!(finalizer.preload_task_count().await, 1);
    }

    #[tokio::test]
    async fn test_handle_signature_adds_to_collector() {
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer(pending_registry);

        let sig = TransactionSignature {
            transaction_id: b"test_tx_001".to_vec(),
            env_id: b"test_env".to_vec(),
            transaction_hash: vec![1, 2, 3],
            signature: vec![4, 5, 6, 7],
            current_stake: 10,
        };
        let body = serialize_to_bytes_rmp(&sig).unwrap();
        let message = Message {
            chain_id: "test_env".to_string(),
            action: String::from("Sign"),
            body,
            signature: vec![],
            public_key: b"executor_1".to_vec(),
            stake_set: None,
        };

        let result = finalizer.handle_signature(&message).await;
        // OPTIMISTIC: First signature triggers immediate optimistic finalize,
        // which cleans up the signature registry. Count is 0 after finalize.
        assert!(result.is_ok());
        assert_eq!(finalizer.signature_count("test_tx_001"), 0);
    }

    #[tokio::test]
    async fn test_handle_signature_optimistic_first_sig() {
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer(pending_registry);

        let sig = TransactionSignature {
            transaction_id: b"test_tx_001".to_vec(),
            env_id: b"test_env".to_vec(),
            transaction_hash: vec![1, 2, 3],
            signature: vec![4, 5, 6, 7],
            current_stake: 10,
        };
        let body = serialize_to_bytes_rmp(&sig).unwrap();
        let message = Message {
            chain_id: "test_env".to_string(),
            action: String::from("Sign"),
            body,
            signature: vec![],
            public_key: b"executor_1".to_vec(),
            stake_set: None,
        };

        let result = finalizer.handle_signature(&message).await;
        // First signature → optimistic finalize succeeds
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_shutdown() {
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer(pending_registry);

        assert!(!finalizer.is_shutting_down().await);

        finalizer.initiate_shutdown().await;
        assert!(finalizer.is_shutting_down().await);
    }

    fn make_stake_set(stakes: Vec<(Vec<u8>, u64)>) -> StakeSet {
        StakeSet {
            stakers: stakes.into_iter().collect(),
        }
    }

    /// Build a block with a real `current_hash` (computed via
    /// `BlockFactory`) so a token's chain validates as a normal (non-empty,
    /// valid) chain.
    fn make_hashed_block(previous_hash: Vec<u8>) -> Block {
        let mut block = Block {
            signed_trans: SignedTransaction::test_transaction(),
            token_metadata: HashMap::new(),
            previous_hash,
            current_hash: vec![],
            timestamp: 1000,
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        block.current_hash = BlockFactory::create_hash(&block);
        block
    }

    #[tokio::test]
    async fn test_stake_cache_fetched_from_data_provider() {
        let pending_registry = make_test_pending_registry();
        let data_provider = Arc::new(
            StubDataProvider::new().with_stake_snapshot(0, make_stake_set(vec![(b"executor_1".to_vec(), 100)])),
        );
        let finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

        // First call — DataProvider fallback, result cached locally
        let fetched = finalizer.get_stake_set_for_epoch().unwrap();
        assert_eq!(fetched.total_stake(), 100);
        assert_eq!(finalizer.stake_cache.cached_count(), 1);

        // Second call — local cache hit, same stake set
        let fetched = finalizer.get_stake_set_for_epoch().unwrap();
        assert_eq!(fetched.total_stake(), 100);
    }

    #[tokio::test]
    async fn test_stake_cache_invalidated_on_advance_epoch() {
        let pending_registry = make_test_pending_registry();
        let data_provider = Arc::new(
            StubDataProvider::new()
                .with_stake_snapshot(0, make_stake_set(vec![(vec![1], 100)]))
                .with_stake_snapshot(1, make_stake_set(vec![(vec![2], 200)])),
        );
        let mut finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

        // Prime the cache with epoch 0's snapshot
        assert_eq!(finalizer.get_stake_set_for_epoch().unwrap().total_stake(), 100);
        assert_eq!(finalizer.stake_cache.cached_count(), 1);

        finalizer.advance_epoch();
        assert_eq!(finalizer.stake_cache.cached_count(), 0);

        // Next fetch pulls the new epoch's snapshot fresh from the DataProvider
        assert_eq!(finalizer.get_stake_set_for_epoch().unwrap().total_stake(), 200);
        assert_eq!(finalizer.stake_cache.cached_count(), 1);
    }

    #[test]
    fn test_get_stake_set_falls_back_to_manual_override() {
        let pending_registry = make_test_pending_registry();
        let data_provider = Arc::new(
            StubDataProvider::new().with_stake_snapshot(0, make_stake_set(vec![(vec![1], 100)])),
        );
        let mut finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

        // Manual override takes priority over the cache (backward compat for tests)
        finalizer.set_stake_set(make_stake_set(vec![(vec![9], 50)]));
        assert_eq!(finalizer.get_stake_set_for_epoch().unwrap().total_stake(), 50);
    }

    #[test]
    fn test_resolve_previous_hash_returns_chain_tip() {
        let pending_registry = make_test_pending_registry();
        let expected;
        let token = {
            let mut token = Token::new().with_id(vec![0, 1, 2]);
            token.blockchain.add_block(make_hashed_block(vec![]));
            expected = token.blockchain.get_current_chain_state().last_hash_in;
            token
        };
        assert!(!expected.is_empty());
        let data_provider = Arc::new(
            StubDataProvider::new().with_token(vec![0, 1, 2], "test_env".to_string(), token),
        );
        let finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

        assert_eq!(finalizer.resolve_previous_hash(&vec![0, 1, 2]), expected);
    }

    #[test]
    fn test_resolve_previous_hash_empty_chain() {
        let pending_registry = make_test_pending_registry();
        let data_provider = Arc::new(
            StubDataProvider::new()
                .with_token(vec![0, 1, 2], "test_env".to_string(), Token::new().with_id(vec![0, 1, 2])),
        );
        let finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

        // Empty chain → last_hash_in is vec![] (natural genesis value)
        assert_eq!(finalizer.resolve_previous_hash(&vec![0, 1, 2]), Vec::<u8>::new());
    }

    #[test]
    fn test_resolve_previous_hash_missing_token_falls_back() {
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer(pending_registry); // empty DataProvider

        // Token not in the provider → graceful fallback to empty prev-hash
        assert_eq!(finalizer.resolve_previous_hash(&vec![0, 1, 2]), Vec::<u8>::new());
    }

    #[test]
    fn test_resolve_previous_hash_invalid_chain_falls_back() {
        let pending_registry = make_test_pending_registry();
        let token = {
            let mut token = Token::new().with_id(vec![0, 1, 2]);
            let mut block = make_hashed_block(vec![]);
            // Deliberately corrupt the hash so the chain validates as invalid
            block.current_hash = vec![9, 9, 9];
            token.blockchain.add_block(block);
            token
        };
        let data_provider = Arc::new(
            StubDataProvider::new().with_token(vec![0, 1, 2], "test_env".to_string(), token),
        );
        let finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

        // ChainState::invalid() → last_hash_in is vec![]
        assert_eq!(finalizer.resolve_previous_hash(&vec![0, 1, 2]), Vec::<u8>::new());
    }

    #[test]
    fn test_resolve_stake_metrics() {
        let pending_registry = make_test_pending_registry();
        let mut finalizer = make_finalizer(pending_registry);

        // No stake data → (0, 0)
        assert_eq!(finalizer.resolve_stake_metrics(), (0, 0));

        // Stake set with two stakers → (300, 2)
        finalizer.set_stake_set(make_stake_set(vec![(vec![1], 100), (vec![2], 200)]));
        assert_eq!(finalizer.resolve_stake_metrics(), (300, 2));
    }

    #[tokio::test]
    async fn test_optimistic_finalize_with_seeded_token() {
        let pending_registry = make_test_pending_registry();
        let data_provider = Arc::new(
            StubDataProvider::new()
                .with_token(vec![0, 1, 2], "test_env".to_string(), Token::new().with_id(vec![0, 1, 2])),
        );
        let finalizer = make_finalizer_with_data_provider(pending_registry, data_provider);

        let sig = TransactionSignature {
            transaction_id: b"test_tx_001".to_vec(),
            env_id: b"test_env".to_vec(),
            transaction_hash: vec![1, 2, 3],
            signature: vec![4, 5, 6, 7],
            current_stake: 10,
        };
        let body = serialize_to_bytes_rmp(&sig).unwrap();
        let message = Message {
            chain_id: "test_env".to_string(),
            action: String::from("Sign"),
            body,
            signature: vec![],
            public_key: b"executor_1".to_vec(),
            stake_set: None,
        };

        // First signature → optimistic finalize, now with a real previous_hash
        // lookup against the seeded token
        assert!(finalizer.handle_signature(&message).await.is_ok());
    }
}
