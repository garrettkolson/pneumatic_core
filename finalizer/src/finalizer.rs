use std::collections::HashMap;
use std::sync::Arc;

use ed25519_dalek::{SigningKey, VerifyingKey};
use tokio::sync::Mutex;

use pneumatic_core::blocks::Block;
use pneumatic_core::config::Config;
use pneumatic_core::crypto::{AsymCryptoProvider, HashProvider};
use pneumatic_core::data::DataProvider;
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::errors::{PneumaticError, ReconciledSignatures};
use pneumatic_core::epoch::StakeSet;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::registry::{PendingTransactionRegistry, TransactionSignatureRegistry};
use pneumatic_core::rns::identity::NodeIdentity;
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
    /// Ed25519 identity provider, retained (cloned from the constructor arg) so the
    /// finalizer can verify inbound envelope and per-transaction signatures against
    /// the voter's public key. See `authenticate_signature_message`.
    identity: Arc<NodeIdentity>,
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
        identity: Arc<NodeIdentity>,
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
        // Retain a clone of the identity for inbound signature verification
        // (`authenticate_signature_message`); the dispatcher keeps the other half
        // for signing outbound messages.
        let finalizer_identity = identity.clone();
        let message_dispatcher = MessageDispatcher::new(
            node_registry.clone(),
            env_id.clone(),
            public_key.clone(),
            identity,
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
            identity: finalizer_identity,
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

    /// Authenticate an inbound voter (executor) message.
    ///
    /// Implements the finalizer side of audit finding C1: the voter's identity is
    /// the public key *proven* by the envelope signature, never the self-declared
    /// `message.public_key` used for routing. Returns the voter's public key on
    /// success, or `PneumaticError` (fail closed) if the envelope signature does
    /// not verify or the key is not registered as an `Executor`.
    ///
    /// This is the single chokepoint every voter signature passes through
    /// (`handle_signature`), so the registered-`Executor` requirement here
    /// prevents any unregistered key from ever entering the signature registry.
    fn authenticate_signature_message(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
        // (1) Envelope signature: the sender's Ed25519 signature over `body`,
        //     verified against the claimed `public_key`. `check_signature` is pure
        //     and returns `Ok(false)` (never panics) on malformed input.
        if !self
            .identity
            .ed25519
            .check_signature(&message.signature, &message.public_key, &message.body)
            .map_err(|e| {
                PneumaticError::CryptoError(format!(
                    "envelope signature verification failed for {}: {e}",
                    bytes_to_hex(&message.public_key)
                ))
            })?
        {
            return Err(PneumaticError::CryptoError(format!(
                "envelope signature verification failed for {}",
                bytes_to_hex(&message.public_key)
            )));
        }

        // (2) Role gate: the verified signer must be registered as an `Executor`.
        match self
            .node_registry
            .find_node_type_by_public_key(&message.public_key)
        {
            Some(NodeRegistryType::Executor) => Ok(message.public_key.clone()),
            Some(other_role) => Err(PneumaticError::Registry(format!(
                "sender {} is registered as {:?}, not an Executor",
                bytes_to_hex(&message.public_key),
                other_role
            ))),
            None => Err(PneumaticError::Registry(format!(
                "sender {} is not registered as any node",
                bytes_to_hex(&message.public_key)
            ))),
        }
    }

    /// Resolve a voter's stake from the current epoch's snapshot.
    ///
    /// Returns the voter's recorded stake, or `0` if the snapshot is unavailable
    /// or the voter has no recorded stake. Used to stamp `current_stake` on an
    /// admitted signature so stake-weighted reconciliation can never trust a
    /// self-reported stake from the message.
    fn current_stake_for_voter(&self, voter_pubkey: &[u8]) -> u64 {
        match self.get_stake_set_for_epoch() {
            Some(set) => set.stakers.get(voter_pubkey).copied().unwrap_or(0),
            None => 0,
        }
    }

    /// Handle a Preload message from the Sentinel/Executor.
    ///
    /// Receives preloaded transaction data and stores it for later processing.
    /// Returns an acknowledgement message.
    pub async fn handle_preload(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
        // Deserialize the transaction payload
        let tx: Transaction = deserialize_rmp_to(&message.body)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Store as a preload task. We persist the *serialized transaction*, not
        // the envelope signature — after Phase 1.1 `message.signature` is a live
        // 64-byte Ed25519 signature, and storing it here would masquerade as
        // preload payload. The sender of preload is Sentinel/Executor and is
        // out of scope for C1 (envelope auth is NOT added to handle_preload in
        // this phase).
        self.preload_tasks
            .lock()
            .await
            .insert(tx.id.clone(), serialize_to_bytes_rmp(&tx)?);

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
        let mut sig: TransactionSignature = deserialize_rmp_to(&message.body)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Extract transaction ID from the signature
        let tx_id = String::from_utf8_lossy(&sig.transaction_id).to_string();

        // (C1) Authenticate the voter. The key that enters the signature registry —
        // and that the optimistic path credits — is the public key proven by the
        // envelope signature and confirmed as a registered `Executor`. Fails
        // closed on any anomaly.
        let voter_pubkey = self.authenticate_signature_message(message)?;

        // (C1) Verify the inner signature actually signs this voter's claimed
        // transaction hash with their key. Rejects any voter whose claimed
        // signature does not verify over the claimed hash. Mirrors the envelope
        // check above: `check_signature` returns `Ok(false)` on a mismatch, so
        // the result must be negated (a bare `?` would only propagate a provider
        // error and silently accept a failing verification).
        if !self
            .identity
            .ed25519
            .check_signature(&sig.signature, &voter_pubkey, &sig.transaction_hash)
            .map_err(|e| {
                PneumaticError::CryptoError(format!(
                    "executor signature verification failed for {}: {e}",
                    bytes_to_hex(&voter_pubkey)
                ))
            })?
        {
            return Err(PneumaticError::CryptoError(format!(
                "executor signature verification failed for {}",
                bytes_to_hex(&voter_pubkey)
            )));
        }

        // (C1) Stamp the voter's real stake from the epoch snapshot — never the
        // self-reported stake carried in the message.
        sig.current_stake = self.current_stake_for_voter(&voter_pubkey);

        // Add the signature to the collector. This is the only path a key enters
        // the registry, and the key is already authenticated.
        self.signature_collector
            .add_signature(&tx_id, voter_pubkey.clone(), sig.clone())?;

        // OPTIMISTIC: First valid signature → try optimistic finalize immediately.
        // The signer is now authenticated + registered, so an attacker cannot
        // forge a registered executor's signature to trigger an optimistic commit.
        if self.signature_collector.signature_count(&tx_id) == 1 {
            return self
                .try_finalize_optimistic(&tx_id, &sig, &voter_pubkey)
                .await;
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
    ///
    /// `voter_pubkey` is the *authenticated* voter (verified in `handle_signature`):
    /// the envelope signature verified and the key is a registered `Executor`.
    async fn try_finalize_optimistic(
        &self,
        tx_id: &str,
        single_sig: &TransactionSignature,
        voter_pubkey: &[u8],
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

        // Step 3: Build SignedTransaction using the single authenticated voter's
        // signature.
        let mut signed_tx = self.block_builder.build_signed_transaction_optimistic(
            single_sig,
            &transaction,
            voter_pubkey,
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
    use pneumatic_core::crypto::{AsymCryptoProvider, BasicHashProvider};
    use pneumatic_core::data::StubDataProvider;
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::blocks::{BlockFactory, FinalityStatus};
    use pneumatic_core::node::{NodeRegistryType, NodeType};
    use pneumatic_core::tokens::Token;
    use pneumatic_core::transactions::{PendingTransaction, TransactionState};
    use pneumatic_core::rns::identity::NodeIdentity;
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
        let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
        let public_key = identity.ed25519.public_key().unwrap_or_default();
        let rhash = identity.rhash;
        Config {
            public_key,
            ip_address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Finalizer],
            main_environment_id: "test_env".to_string(),
            reconciliation_partition_id: "reconciliation".to_string(),
            environment_metadata: make_test_env_data(),
            type_configs: Arc::new(DashMap::new()),
            identity: Arc::new(identity),
            rhash,
            bootstrap_peers: Vec::new(),
            rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
            transport_enabled: false,
        }
    }

    fn make_test_node_registry() -> Arc<NodeRegistry> {
        let config = make_test_config();
        Arc::new(NodeRegistry::init(
            Arc::new(config),
            None,
            Arc::new(|_, _| true),
        ))
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
            sender_signature: vec![],
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
        make_finalizer_with_registry_and_data_provider(
            make_test_node_registry(),
            pending_registry,
            Arc::new(StubDataProvider::new()),
        )
    }

    /// Factory that wires a DataProvider with a pre-seeded stake snapshot
    /// for the given epoch, so stake fetching tests can verify the
    /// `BlockFinalized` gossip path.
    fn make_finalizer_with_data_provider(
        pending_registry: Arc<PendingTransactionRegistry>,
        data_provider: Arc<StubDataProvider>,
    ) -> Finalizer {
        make_finalizer_with_registry_and_data_provider(
            make_test_node_registry(),
            pending_registry,
            data_provider,
        )
    }

    /// Build a finalizer wired to a caller-supplied `node_registry` and
    /// `data_provider`. Used by the auth-gate regression tests, which need to
    /// register executor voters in the registry before constructing the
    /// finalizer.
    fn make_finalizer_with_registry_and_data_provider(
        node_registry: Arc<NodeRegistry>,
        pending_registry: Arc<PendingTransactionRegistry>,
        data_provider: Arc<StubDataProvider>,
    ) -> Finalizer {
        let signature_registry = Arc::new(TransactionSignatureRegistry::new());
        let hash_provider = Arc::new(BasicHashProvider::new());

        let (signing_key, verifying_key) = make_test_signing_key();

        Finalizer::new(
            "test_env".to_string(),
            vec![1, 2, 3, 4],
            Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
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

    /// Register `voter` as an `Executor` in `registry` so the finalizer's auth
    /// gate (`find_node_type_by_public_key`) recognizes it. The connection is a
    /// no-op — the auth gate only needs the key to be present in the Executor
    /// shard. Executor capacity must be configured; an unconfigured type has
    /// `get_max_node_number == 0` and `register_peer` rejects every peer.
    fn register_executor(registry: &Arc<NodeRegistry>, voter: &NodeIdentity) {
        registry
            .get_config()
            .type_configs
            .insert(
                NodeRegistryType::Executor,
                pneumatic_core::node::NodeTypeConfig { min: 0, max: 10, min_stake: 0 },
            );
        let pk = voter.ed25519.public_key().expect("voter public key");
        assert!(
            registry.register_peer(pk, voter.rhash, &NodeRegistryType::Executor, Box::new(NoOpConnection)),
            "executor should register within capacity"
        );
    }

    /// A `Connection` that discards sent data. Only needed because
    /// `NodeRegistry::register_peer` requires a `Box<dyn Connection>`; the
    /// finalizer's auth gate never sends over it.
    struct NoOpConnection;

    #[async_trait::async_trait]
    impl pneumatic_core::conns::Connection for NoOpConnection {
        async fn send(&self, _data: &Vec<u8>) -> Result<(), pneumatic_core::conns::ConnError> {
            Ok(())
        }
    }

    /// Build an authenticated `Sign` message from `voter` for `tx_id` over
    /// `transaction_hash`. Both the envelope signature and the inner
    /// `TransactionSignature.signature` are real Ed25519 signatures under
    /// `voter`'s key, so the finalizer's auth gate accepts the message.
    fn build_signed_sign_message(
        chain_id: &str,
        tx_id: &[u8],
        transaction_hash: Vec<u8>,
        current_stake: u64,
        voter: &NodeIdentity,
    ) -> Message {
        let inner_signature = voter
            .ed25519
            .sign_data(&transaction_hash)
            .expect("voter signs transaction hash");
        let sig = TransactionSignature {
            transaction_id: tx_id.to_vec(),
            env_id: chain_id.as_bytes().to_vec(),
            transaction_hash,
            signature: inner_signature,
            current_stake,
        };
        let body = serialize_to_bytes_rmp(&sig).expect("serialize signature");
        Message::signed(chain_id.to_string(), "Sign", body, None, voter)
            .expect("sign envelope")
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
            sender_signature: vec![],
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
        let node_registry = make_test_node_registry();
        let voter = NodeIdentity::generate_in_memory();
        register_executor(&node_registry, &voter);
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer_with_registry_and_data_provider(
            node_registry,
            pending_registry,
            Arc::new(StubDataProvider::new()),
        );

        let message =
            build_signed_sign_message("test_env", b"test_tx_001", vec![1, 2, 3], 10, &voter);

        let result = finalizer.handle_signature(&message).await;
        // OPTIMISTIC: First (authenticated) signature triggers immediate
        // optimistic finalize, which cleans up the signature registry. Count is
        // 0 after finalize.
        assert!(result.is_ok());
        assert_eq!(finalizer.signature_count("test_tx_001"), 0);
    }

    #[tokio::test]
    async fn test_handle_signature_optimistic_first_sig() {
        let node_registry = make_test_node_registry();
        let voter = NodeIdentity::generate_in_memory();
        register_executor(&node_registry, &voter);
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer_with_registry_and_data_provider(
            node_registry,
            pending_registry,
            Arc::new(StubDataProvider::new()),
        );

        let message =
            build_signed_sign_message("test_env", b"test_tx_001", vec![1, 2, 3], 10, &voter);

        let result = finalizer.handle_signature(&message).await;
        // First authenticated signature → optimistic finalize succeeds.
        assert!(result.is_ok());
    }

    // --- Phase 1.4 / audit finding C1 regression tests -----------------------
    //
    // Every test below asserts on what the *fix* restores and on what the
    // pre-fix code (which trusted the self-declared `message.public_key`)
    // violated. With an empty pending registry, an optimistic finalize fails
    // before cleanup, so an admitted signature persists in the registry for
    // inspection — and a rejected voter's signature never enters it.
    //
    // Each fails WITHOUT the fix: the old `handle_signature` never verified the
    // envelope signature, never consulted the registry, never verified the inner
    // signature, and never stamped stake from the snapshot.

    #[tokio::test]
    async fn forged_voter_key_rejected() {
        // The body is signed by `attacker`, but the message claims a *registered*
        // voter's public key. Without the fix, `handle_signature` trusted the
        // claimed `public_key` and admitted the signature for that voter.
        let node_registry = make_test_node_registry();
        let voter = NodeIdentity::generate_in_memory();
        register_executor(&node_registry, &voter);
        let attacker = NodeIdentity::generate_in_memory();
        let voter_pk = voter.ed25519.public_key().expect("voter public key");
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer_with_registry_and_data_provider(
            node_registry,
            pending_registry,
            Arc::new(StubDataProvider::new()),
        );

        let inner = voter.ed25519.sign_data(&[1, 2, 3]).expect("voter signs inner");
        let sig = TransactionSignature {
            transaction_id: b"forged_tx".to_vec(),
            env_id: b"test_env".to_vec(),
            transaction_hash: vec![1, 2, 3],
            signature: inner,
            current_stake: 10,
        };
        let body = serialize_to_bytes_rmp(&sig).expect("serialize inner");
        // Envelope signed by the attacker, claiming the voter's key.
        let message = Message {
            chain_id: "test_env".to_string(),
            action: "Sign".to_string(),
            body,
            signature: attacker.ed25519.sign_data(&Vec::new()).expect("attacker signs"),
            public_key: voter_pk.clone(),
            stake_set: None,
        };

        let result = finalizer.handle_signature(&message).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), pneumatic_core::errors::PneumaticError::CryptoError(_))
        );
        // The forged identity must not have been admitted to the registry.
        assert!(
            finalizer
                .signature_registry
                .get_transaction_registry("forged_tx")
                .is_none(),
            "attacker impersonating a registered voter must not be admitted"
        );
    }

    #[tokio::test]
    async fn missing_or_invalid_envelope_signature_rejected() {
        // A `Sign` message with an empty envelope signature. Without the fix the
        // handler ignored the signature entirely and cached the self-declared key.
        let node_registry = make_test_node_registry();
        let voter = NodeIdentity::generate_in_memory();
        register_executor(&node_registry, &voter);
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer_with_registry_and_data_provider(
            node_registry,
            pending_registry,
            Arc::new(StubDataProvider::new()),
        );

        let mut message =
            build_signed_sign_message("test_env", b"nosig_tx", vec![1, 2, 3], 10, &voter);
        message.signature = Vec::new();

        let result = finalizer.handle_signature(&message).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), pneumatic_core::errors::PneumaticError::CryptoError(_))
        );
        assert!(
            finalizer
                .signature_registry
                .get_transaction_registry("nosig_tx")
                .is_none(),
            "a message with no verifiable signature must not be admitted"
        );
    }

    #[tokio::test]
    async fn non_registered_voter_rejected() {
        // A valid envelope, but the signer is not registered as any node — the
        // core C1 rejection. Without the fix the self-declared key was silently
        // admitted into the check-or-create signature registry.
        let node_registry = make_test_node_registry();
        let voter = NodeIdentity::generate_in_memory(); // deliberately NOT registered
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer_with_registry_and_data_provider(
            node_registry,
            pending_registry,
            Arc::new(StubDataProvider::new()),
        );

        let message = build_signed_sign_message("test_env", b"noreg_tx", vec![1, 2, 3], 10, &voter);

        let result = finalizer.handle_signature(&message).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), pneumatic_core::errors::PneumaticError::Registry(_))
        );
        assert!(
            finalizer
                .signature_registry
                .get_transaction_registry("noreg_tx")
                .is_none(),
            "an unregistered voter must not pollute the signature registry"
        );
    }

    #[tokio::test]
    async fn bad_inner_signature_rejected() {
        // Registered Executor and a valid envelope, but the inner
        // `TransactionSignature.signature` does not verify over `transaction_hash`.
        // Without the fix the inner signature was never verified at all.
        let node_registry = make_test_node_registry();
        let voter = NodeIdentity::generate_in_memory();
        register_executor(&node_registry, &voter);
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer_with_registry_and_data_provider(
            node_registry,
            pending_registry,
            Arc::new(StubDataProvider::new()),
        );

        // Inner signature verifies over [9,9,9], but claims transaction_hash = [1,2,3].
        let inner = voter.ed25519.sign_data(&vec![9, 9, 9]).expect("inner signs wrong hash");
        let body = serialize_to_bytes_rmp(&TransactionSignature {
            transaction_id: b"bad_inner_tx".to_vec(),
            env_id: b"test_env".to_vec(),
            transaction_hash: vec![1, 2, 3],
            signature: inner,
            current_stake: 10,
        })
        .expect("serialize inner");
        // Envelope itself is a valid signature of `body` by the registered voter.
        let message = Message::signed(String::from("test_env"), "Sign", body, None, &voter).expect("sign envelope");

        let result = finalizer.handle_signature(&message).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), pneumatic_core::errors::PneumaticError::CryptoError(_))
        );
        assert!(
            finalizer
                .signature_registry
                .get_transaction_registry("bad_inner_tx")
                .is_none(),
            "a voter whose inner signature does not verify must not be admitted"
        );
    }

    #[tokio::test]
    async fn quorum_counts_only_verified_registered_voters() {
        // Three voters: A and B are registered Executors, C is not. A and B are
        // admitted (and persist because the empty-pending optimistic path fails
        // before cleanup); C is rejected at the auth gate. Without the fix all
        // three would be admitted via the self-declared key.
        let node_registry = make_test_node_registry();
        let a = NodeIdentity::generate_in_memory();
        let b = NodeIdentity::generate_in_memory();
        let c = NodeIdentity::generate_in_memory(); // unregistered
        register_executor(&node_registry, &a);
        register_executor(&node_registry, &b);
        let a_pk = a.ed25519.public_key().expect("a public key");
        let b_pk = b.ed25519.public_key().expect("b public key");
        let c_pk = c.ed25519.public_key().expect("c public key");
        let pending_registry = make_test_pending_registry();
        let finalizer = make_finalizer_with_registry_and_data_provider(
            node_registry,
            pending_registry,
            Arc::new(StubDataProvider::new()),
        );

        let tx = b"quorum_tx";
        let sig_a = build_signed_sign_message("test_env", tx, vec![1, 2, 3], 10, &a);
        let sig_b = build_signed_sign_message("test_env", tx, vec![1, 2, 3], 10, &b);
        let sig_c = build_signed_sign_message("test_env", tx, vec![1, 2, 3], 10, &c);

        // A: admitted, then optimistic fails (no pending tx) — signature persists.
        let _ = finalizer.handle_signature(&sig_a).await;
        // B: admitted (quorum-accumulate path, no optimistic).
        let _ = finalizer.handle_signature(&sig_b).await;
        // C: rejected at the auth gate — never reaches the registry.
        assert!(finalizer.handle_signature(&sig_c).await.is_err());

        let registry = finalizer
            .signature_registry
            .get_transaction_registry("quorum_tx")
            .expect("transaction entry exists");
        assert_eq!(
            registry.len(),
            2,
            "only the two registered+verified voters count toward quorum"
        );
        assert!(registry.contains_key(&a_pk));
        assert!(registry.contains_key(&b_pk));
        assert!(
            !registry.contains_key(&c_pk),
            "an unregistered voter must not advance the count"
        );
    }

    #[tokio::test]
    async fn current_stake_comes_from_snapshot_not_message() {
        // The voter injects a bogus current_stake (u64::MAX). The fix stamps the
        // voter's real stake from the epoch snapshot before admitting the
        // signature; without the fix the self-reported stake would be trusted.
        let node_registry = make_test_node_registry();
        let voter = NodeIdentity::generate_in_memory();
        register_executor(&node_registry, &voter);
        let voter_pk = voter.ed25519.public_key().expect("voter public key");
        let pending_registry = make_test_pending_registry();
        let data_provider = Arc::new(
            StubDataProvider::new()
                .with_stake_snapshot(0, make_stake_set(vec![(voter_pk.clone(), 100)])),
        );
        let finalizer = make_finalizer_with_registry_and_data_provider(
            node_registry,
            pending_registry,
            data_provider,
        );

        let message = build_signed_sign_message("test_env", b"stake_tx", vec![1, 2, 3], u64::MAX, &voter);
        let _ = finalizer.handle_signature(&message).await;

        let registry = finalizer
            .signature_registry
            .get_transaction_registry("stake_tx")
            .expect("transaction entry exists");
        let stored = registry.get(&voter_pk).expect("voter signature persisted");
        assert_eq!(
            stored.current_stake, 100,
            "stake must be taken from the epoch snapshot"
        );
        assert_ne!(
            stored.current_stake,
            u64::MAX,
            "a self-reported stake from the message must never be trusted"
        );
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
        let node_registry = make_test_node_registry();
        let voter = NodeIdentity::generate_in_memory();
        register_executor(&node_registry, &voter);
        let data_provider = Arc::new(
            StubDataProvider::new()
                .with_token(vec![0, 1, 2], "test_env".to_string(), Token::new().with_id(vec![0, 1, 2])),
        );
        let finalizer = make_finalizer_with_registry_and_data_provider(
            node_registry,
            make_test_pending_registry(),
            data_provider,
        );

        // An authenticated, registered executor's honest first signature. The
        // pre-fix code trusted a self-declared `public_key` with an empty
        // envelope signature; Phase 1.4 requires the envelope signature to verify
        // and the voter to be registered as an Executor. The real intent of this
        // test — optimistic finalize with a real previous_hash lookup against a
        // seeded token — is unchanged.
        let message = build_signed_sign_message("test_env", b"test_tx_001", vec![1, 2, 3], 10, &voter);

        // First signature → optimistic finalize, now with a real previous_hash
        // lookup against the seeded token
        assert!(finalizer.handle_signature(&message).await.is_ok());
    }
}
