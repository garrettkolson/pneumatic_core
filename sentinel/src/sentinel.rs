use std::sync::Arc;

use pneumatic_core::blocks::Block;
use pneumatic_core::config::{Config, meets_minimum_stake};
use pneumatic_core::data::DataProvider;
use pneumatic_core::encoding::deserialize_rmp_to;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::epoch::{EpochSnapshotCache, ExecutorSet, FINALIZER_DOMAIN, StakeSet};
use pneumatic_core::errors::{PneumaticError, ValidationFailureReason};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::messages::Message;
use pneumatic_core::node::{NodeRegistryRequest, NodeRegistryType, registry::NodeRegistry};
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::shielded::ShieldedPoolView;
use pneumatic_core::transactions::{ShieldedTransaction, Transaction, TransactionState};
use pneumatic_core::validation::{ShieldedValidationDeps, ShieldedValidationSpec};

use crate::sentinel_error::SentinelError;



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
    stake_snapshot_cache: Arc<EpochSnapshotCache<StakeSet>>,
    /// Executor set cache for deterministic shard-aware routing.
    /// Loaded from local cache → DataProvider → (reserved) peer fallback.
    executor_set_cache: Arc<EpochSnapshotCache<ExecutorSet>>,
    /// The environment this sentinel operates on.
    env_data: Arc<EnvironmentMetadata>,
    /// The read-only shielded-pool view the advisory shielded validation
    /// reads (Phase S5.1, *Decision 1*): the spent-nullifier set + committed-
    /// root history, bundled so one sentinel can never read two different
    /// pools. S5.4 swaps the one construction site for the real
    /// `Arc<ShieldedPool>`; this sentinel never changes. Required, not
    /// `Option`: a pristine `SimpleShieldedPoolView` fails closed on its own
    /// (*Decision 2*).
    pool_view: Arc<dyn ShieldedPoolView>,
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
        pool_view: Arc<dyn ShieldedPoolView>,
    ) -> Self {
        let transaction_notifier = Arc::new(
            super::transaction_notifier::TransactionNotifier::new(config, Arc::clone(&node_registry))
        );
        let validator = super::transaction_validator::TransactionValidator::new(env_data.clone(), Arc::clone(&data_provider));
        let partition_id = env_data.environment_id.clone();
        // Each fetch closure captures its own clone of the provider Arc so
        // the `data_provider` field can still be moved into the struct below.
        let stake_dp = data_provider.clone();
        let stake_snapshot_cache = Arc::new(EpochSnapshotCache::new(
            partition_id.clone(),
            move |epoch, partition| stake_dp.get_stake_snapshot(epoch, partition),
        ));
        let executor_dp = data_provider.clone();
        let executor_set_cache = Arc::new(EpochSnapshotCache::new(
            partition_id,
            move |epoch, partition| executor_dp.get_executor_set(epoch, partition),
        ));

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
            pool_view,
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
            // Shielded value transfer (Phase S3.2 routing, S5.1 real path).
            // Inner action under the `"Verify"` envelope — identical shape to
            // how `"Process"` arrives today (the composite Sentinel plugin
            // calls `on_data_received` with the inner message body, ignoring
            // the outer `"Verify"` action that `SENTINEL_ACTIONS` already
            // gated).
            "ShieldedTransfer" => self.handle_shielded_transfer(message),
            action => Err(SentinelError::UnknownAction(action.to_string())),
        }
    }

}

// Production logic split out of the `impl Sentinel` body above, into descendant modules so
// this file stays focused on the struct, construction, and action routing. Each module
// re-declares `impl Sentinel` for its concern area and pulls in this module's items
// (Sentinel, SentinelError, the use-imported types) via `use super::*;`.
pub mod processing;
pub mod finalizing;
pub mod registering;
pub mod epoching;
pub mod shielded;

// The per-domain test modules and the shared `helpers` module now live under
// src/sentinel/ as child modules of `tests`. Because they are descendants of
// `crate::sentinel`, they retain access to the Sentinel's private fields with
// no accessor changes.
#[cfg(test)]
mod tests {
    pub mod helpers;
    mod processing;
    mod finalizing;
    mod registering;
    mod epoching;
    mod shielded;
    mod error;
}

