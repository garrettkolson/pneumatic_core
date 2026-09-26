use std::collections::HashMap;
use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use tokio::sync::Mutex;

use pneumatic_core::crypto::{AsymCryptoProvider, HashProvider};
use pneumatic_core::data::DataProvider;
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::errors::{PneumaticError, ValidationFailureReason};
use pneumatic_core::epoch::{EpochSnapshotCache, StakeSet};
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::registry::{PendingTransactionRegistry, TransactionSignatureRegistry};
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::shielded::ShieldedPoolView;
use pneumatic_core::transactions::{
    ShieldedTransaction, Transaction, TransactionCommit, TransactionSignature, TransactionState,
};
use pneumatic_core::validation::{ShieldedValidationDeps, ShieldedValidationSpec};

use crate::block_builder::BlockBuilder;
use crate::message_dispatcher::MessageDispatcher;
use crate::signature_collector::SignatureCollector;

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
    stake_cache: EpochSnapshotCache<StakeSet>,
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
    /// Read-only shielded-pool seam (Phase S5.2): the finalizer re-runs the
    /// shielded validation against its *own* view before voting. S5.4 swaps the
    /// construction site for the real `Arc<ShieldedPool>` — the trait is stable.
    pool_view: Arc<dyn ShieldedPoolView>,
    /// Environment metadata — the `Shielded` spec lookup, the `max_risk` gate,
    /// and the `shielded_root_recency` window the advisory validation reads.
    env_data: Arc<EnvironmentMetadata>,
    /// Recorded shielded transfers, keyed by tx id, as their **canonical bytes**
    /// (Phase S5.2). When a `SignShielded` arrives, the finalizer stores the
    /// exact bytes it verified and signed; `try_finalize_shielded` rebuilds the
    /// `ShieldedTransaction` from this store so the block always carries the
    /// byte-identical payload the votes bind to. Never evicted until a
    /// finalization succeeds (mirrors the sentinel's S5.1 map).
    shielded_transactions: Arc<Mutex<HashMap<String, Vec<u8>>>>,
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
    signing_key: Arc<NodeIdentity>,
    verifying_key: VerifyingKey,
    hash_provider: Arc<dyn HashProvider>,
    leader_address: Vec<u8>,
    leader_stake: u64,
    leader_hash: Vec<u8>,
    current_epoch: u64,
    data_provider: Arc<dyn DataProvider>,
    partition_id: String,
    pool_view: Arc<dyn ShieldedPoolView>,
    env_data: Arc<EnvironmentMetadata>,
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
        stake_cache: {
            // The fetch closure captures its own clone of the provider
            // Arc so `data_provider` can still be moved into the struct.
            let stake_dp = data_provider.clone();
            EpochSnapshotCache::new(partition_id.clone(), move |epoch, partition| {
                stake_dp.get_stake_snapshot(epoch, partition)
            })
        },
        stake_set: None,
        data_provider,
        partition_id,
        identity: finalizer_identity,
        pool_view,
        env_data,
        shielded_transactions: Arc::new(Mutex::new(HashMap::new())),
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

pub mod finalizing;
pub mod shielded;
pub mod signing;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    pub mod helpers;
    mod epoch_stake;
    mod shielded;
    mod signing;
}
