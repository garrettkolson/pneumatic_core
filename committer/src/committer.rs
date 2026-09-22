use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::sync::Arc;

use dashmap::{DashMap, Entry};
use tokio::sync::Mutex;

use pneumatic_core::crypto::AsymCryptoProvider;
use pneumatic_core::data::{DataError, DataProvider};
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::blocks::{AppendOutcome, Block, BlockFactory, Blockchain, FinalityStatus};

use pneumatic_core::epoch::{BlockProposer, CandidateRegistry, ConflictResolution, EpochBoundaryDetector, ExecutorSet, IEpochLeaderSelector, IEpochReconciler, IBlockProposer, IStakingManager, StakeSet, resolve_block_conflict};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::logging::Logger;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::tokens::Token;
use pneumatic_core::transactions::{PendingTransaction, SignedTransaction, TransactionCommit, TransactionState};

use super::block_services::BlockServices;
use super::committer_error::CommitterError;
use super::epoch_manager::{EpochReconciler, LeaderSelector, StakeStore, StakingManager};
use super::orphan_buffer::{BufferDecision, OrphanBuffer};
use super::shielded_pool::{PoolApplyOutcome, ShieldedPool};

/// Convert a byte slice to a hex string (lowercase, no prefix).
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Outcome of conflict resolution on the commit path. Returned by
/// [`Committer::handle_conflict_at_commit`] so the caller knows whether to append
/// the incoming block directly, to append it after rolling back the losing tip,
/// or to reject it outright (a rejection is surfaced to the caller as an
/// `Err(CommitterError::LoserDiscarded)`, not this enum). (AUDIT Phase 5.2 / H2)
#[derive(Debug, Clone)]
pub(crate) enum CommitConflictOutcome {
    /// No competing proposal has committed a block for this position — the incoming
    /// block is safe to append as-is.
    Commit,
    /// The incoming block won its conflict, but the losing proposal is the current
    /// chain tip. Roll back `loser_hash` (only if it matches the tip) before appending
    /// the winner, so exactly one block ends up at that position.
    CommitWinnerAfterRollback(Vec<u8>),
}

/// Which node roles are permitted to originate a message of a given action.
/// Derived from the actual wire senders: `Commit`/`BlockFinalized` come only
/// from Finalizers, `DistributeToken`/`DistributeBlock` only from Committers,
/// and `BlockConfirmed`/`BlockQuorumReached` are honest broadcasts from any
/// registered node.
#[derive(Debug, Clone)]
enum AllowedSenders {
    /// Exactly one role may send the action.
    Exact(NodeRegistryType),
    /// Any registered node may send the action.
    AnyRegistered,
    /// Only this committer's own identity may send the action.
    SelfOnly,
}

/// Map an action string to the roles permitted to send it.
fn allowed_senders_for(action: &str) -> AllowedSenders {
    match action {
        "Commit" | "BlockFinalized" => AllowedSenders::Exact(NodeRegistryType::Finalizer),
        "DistributeToken" | "DistributeBlock" => AllowedSenders::Exact(NodeRegistryType::Committer),
        "BlockConfirmed" | "BlockQuorumReached" => AllowedSenders::AnyRegistered,
        "EpochReconcile" => AllowedSenders::SelfOnly,
        // Any unrecognized action falls back to Committer-only so it is still
        // rejected at the dispatch stage as UnknownAction.
        _ => AllowedSenders::Exact(NodeRegistryType::Committer),
    }
}

// ---------------------------------------------------------------------------
// Committer — receives TransactionCommit messages, commits blocks,
//             manages epoch transitions
// ---------------------------------------------------------------------------

/// The Committer is the terminal node in the pneumatic pipeline.
/// It receives `TransactionCommit` messages from the Finalizer,
/// validates and commits blocks to token blockchains, distributes
/// blocks to archivers, and manages epoch transitions.
///
/// Responsibilities:
/// 1. Receive and validate `TransactionCommit` messages (action: "Commit")
/// 2. Commit validated blocks to token blockchains
/// 3. Distribute committed blocks to archivers
/// 4. Handle token distribution from other committers
/// 5. Manage epoch transitions (staking, reconciliation, leader selection)
pub struct Committer {
    /// Environment metadata for validation and logging
    env_data: Arc<EnvironmentMetadata>,
    /// Public key of this committer node
    public_key: Vec<u8>,
    /// Node identity — signs all outgoing broadcast messages
    identity: Arc<NodeIdentity>,
    /// Gossiper for receiving messages
    gossiper: Arc<Gossiper>,
    /// Block services (token commit, block distribution, token distribution)
    block_services: Arc<BlockServices>,
    /// Shared registry of connected nodes
    node_registry: Arc<NodeRegistry>,
    /// Token cache — token_id -> Token, DashMap-backed for concurrency
    tokens: Arc<DashMap<Vec<u8>, Token>>,
    /// Transaction registry for state tracking
    pending_registry: Arc<PendingTransactionRegistry>,
    /// Stake store for epoch management
    stake_store: Arc<StakeStore>,
    /// Staking manager for epoch reconciliation
    staking_manager: Arc<StakingManager>,
    /// Epoch reconciler for chain analysis at epoch boundaries
    epoch_reconciler: Arc<EpochReconciler>,
    /// Leader selector for epoch transitions
    leader_selector: Arc<LeaderSelector>,
    /// Data provider for loading/saving user data (gas deduction)
    data_provider: Arc<dyn DataProvider>,
    /// Current epoch number for deterministic leader selection
    current_epoch_number: AtomicU64,
    /// Flag: is the committer shutting down?
    awaiting_shutdown: Arc<Mutex<bool>>,
    /// Epoch boundary detector — leader checks expiry and advances epochs
    epoch_detector: Arc<Mutex<Option<EpochBoundaryDetector>>>,
    /// Block proposer — dequeues transactions from the pool for batch proposal
    block_proposer: Arc<dyn IBlockProposer>,
    /// Candidate registry for conflict detection at commit time
    candidate_registry: Arc<CandidateRegistry>,
    /// The global shielded pool (S5.3) — the sole owner of committed
    /// shielded state, shared by `Arc` with the block services and (in the
    /// composite node) the shielded roles' validation views.
    shielded_pool: Arc<ShieldedPool>,
    /// Duration of each epoch in seconds
    epoch_duration: i64,
    /// Interval between proposal polls in milliseconds
    proposal_interval_ms: u64,
    /// Per-block confirmation tracking: block_hash -> (confirmed keys, cumulative stake)
    /// Used for quorum gossip to track which nodes have confirmed which blocks.
    confirmation_votes: Mutex<HashMap<Vec<u8>, (HashSet<Vec<u8>>, u64)>>,
    /// Cache of stake sets received via BlockFinalized messages, keyed by block hash.
    /// Used to look up sender stakes when processing BlockConfirmed votes.
    stake_set_cache: Mutex<HashMap<Vec<u8>, StakeSet>>,
    /// Bounded, per-token orphan buffer for finalized blocks received out of order (AUDIT Phase
    /// 3.4 / H15). A BlockFinalized whose block does not chain onto the current tip is buffered
    /// here and replayed as the tip advances, so out-of-order delivery is never silently dropped.
    orphan_blocks: Mutex<OrphanBuffer>,
    /// Per-sender guard serializing the commit-time gas read-modify-write
    /// (`get_user` -> subtract -> `save_user`). Two commits from the same sender
    /// (which run on separate `tokio` tasks) must not race on the shared account
    /// balance or lose an update; different senders stay concurrent (AUDIT Phase
    /// 4.5 / M11). A blocking `std::sync::Mutex` is fine here because the guarded
    /// calls are already blocking data-service reads inside the async handler.
    /// Per-sender map of guards serializing the commit-time gas read-modify-write
    /// (`get_user` -> subtract -> `save_user`). Two commits from the same sender
    /// (which run on separate `tokio` tasks) must not race on the shared account
    /// balance or lose an update; different senders stay concurrent (AUDIT Phase
    /// 4.5 / M11). Keyed by sender public key, mirroring the existing
    /// `confirmation_votes` / `stake_set_cache` fields.
    rmw_locks: Mutex<HashMap<Vec<u8>, Arc<std::sync::Mutex<()>>>>,
}

// Production logic split out of the `impl Committer` body below, into descendant modules so
// this file stays focused on routing and construction. Each module re-declares `impl Committer`
// for its concern area and pulls in crate::committer's items (Committer, CommitConflictOutcome,
// bytes_to_hex, allowed_senders_for, the use-imported types) via `use super::*;`.
pub mod committing;
pub mod distributing;
pub mod finalizing;
pub mod quoruming;
pub mod epoching;

impl Committer {
    /// Create a new Committer with all required components.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        env_data: Arc<EnvironmentMetadata>,
        public_key: Vec<u8>,
        identity: Arc<NodeIdentity>,
        gossiper: Arc<Gossiper>,
        block_services: Arc<BlockServices>,
        node_registry: Arc<NodeRegistry>,
        tokens: Arc<DashMap<Vec<u8>, Token>>,
        pending_registry: Arc<PendingTransactionRegistry>,
        stake_store: Arc<StakeStore>,
        staking_manager: Arc<StakingManager>,
        epoch_reconciler: Arc<EpochReconciler>,
        leader_selector: Arc<LeaderSelector>,
        data_provider: Arc<dyn DataProvider>,
        current_epoch_number: u64,
        epoch_detector: Option<EpochBoundaryDetector>,
        block_proposer: Arc<dyn IBlockProposer>,
        epoch_duration: i64,
        proposal_interval_ms: u64,
        candidate_registry: Arc<CandidateRegistry>,
        shielded_pool: Arc<ShieldedPool>,
    ) -> Self {
        Committer {
            env_data,
            public_key,
            identity,
            gossiper,
            block_services,
            node_registry,
            tokens,
            pending_registry,
            stake_store,
            staking_manager,
            epoch_reconciler,
            leader_selector,
            data_provider,
            current_epoch_number: AtomicU64::new(current_epoch_number),
            awaiting_shutdown: Arc::new(Mutex::new(false)),
            epoch_detector: Arc::new(Mutex::new(epoch_detector)),
            block_proposer,
            epoch_duration,
            proposal_interval_ms,
            candidate_registry,
            shielded_pool,
            confirmation_votes: Mutex::new(HashMap::new()),
            stake_set_cache: Mutex::new(HashMap::new()),
            orphan_blocks: Mutex::new(OrphanBuffer::new(1024, 256, Duration::from_secs(30))),
            rmw_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Initialize the committer — wire the gossiper's message handler.
    /// The closure receives deserialized messages and routes them
    /// to the appropriate `handle_*` method.
    pub fn initialize<F>(&self, on_message_received: F)
    where
        F: Fn(Message) + Send + Sync + 'static,
    {
        // Wrap the caller's Fn(Message) to deserialize from raw bytes.
        // The gossiper stores a Fn(Vec<u8>) and calls this wrapper
        // after deserialization and dedup checks pass.
        let wrapped = move |raw: Vec<u8>| {
            if let Ok(msg) = deserialize_rmp_to(&raw) {
                on_message_received(msg);
            }
            // Silently drop malformed messages — the gossiper already
            // recorded them in the dedup cache.
        };
        self.gossiper.initialize(wrapped);
    }

    /// Set the shutdown flag.
    pub async fn initiate_shutdown(&self) {
        *self.awaiting_shutdown.lock().await = true;
    }

    /// Check if the committer is shutting down.
    pub async fn is_shutting_down(&self) -> bool {
        *self.awaiting_shutdown.lock().await
    }

    /// Primary entry point — routes incoming messages by action.
    /// Called by the gossiper's message handler after deserialization.
    pub async fn handle_message(&self, message: Message) -> Result<(), CommitterError> {
        // Fail-closed sender authentication. The gossiper already verified the
        // envelope signature and deduplicated before forwarding here, but the
        // router is the authoritative auth boundary and must not accept a
        // message from an unregistered sender or a role that may not send this
        // action.
        self.authenticate_message(&message)?;

        match message.action.as_str() {
            "Commit" => self.handle_commit(message).await,
            "DistributeToken" => self.handle_token_distribution(message).await,
            "DistributeBlock" => self.handle_block_distribution(message).await,
            "EpochReconcile" => self.handle_epoch_reconcile().await,
            "BlockFinalized" => self.handle_block_finalized(message).await,
            "BlockConfirmed" => self.handle_block_confirmed_vote(message).await,
            "BlockQuorumReached" => self.handle_block_quorum_reached(message).await,
            action => Err(CommitterError::UnknownAction(action.to_string())),
        }
    }

    /// Fail-closed sender authentication for an incoming message.
    ///
    /// Verifies, in order, that:
    ///   1. the message body is signed by `message.public_key` (envelope
    ///      integrity — defense-in-depth alongside the gossiper's upstream
    ///      signature check);
    ///   2. `message.public_key` identifies a registered node (unregistered
    ///      keys cannot be authenticated); and
    ///   3. that node's role is permitted to send `message.action`.
    ///
    /// Any failure is returned as an error rather than falling through to the
    /// action handler.
    fn authenticate_message(&self, message: &Message) -> Result<(), CommitterError> {
        let crypto = self
            .env_data
            .asym_crypto_provider
            .read()
            .expect("crypto provider poisoned");

        // (1) Envelope signature over the message body.
        if !crypto
            .check_signature(&message.signature, &message.public_key, &message.body)
            .unwrap_or(false)
        {
            return Err(CommitterError::UnauthenticatedSender(bytes_to_hex(&message.public_key)));
        }

        // (2) Registration + role set (Phase 6): is the signer a known node? A
        // composite identity may be registered under several roles — resolve the
        // full set.
        let roles = self
            .node_registry
            .find_node_types_by_public_key(&message.public_key);
        if roles.is_empty() {
            return Err(CommitterError::UnauthenticatedSender(bytes_to_hex(&message.public_key)));
        }

        // (3) Role gate: allowed-role(action) must intersect the node's role
        // set — a composite identity registered for N roles may send actions
        // for any of them. An action whose sole governing role the node is not
        // under is rejected (intersection empty ⇒ fail closed).
        match allowed_senders_for(&message.action) {
            AllowedSenders::Exact(expected) => {
                if self.node_registry.node_may_send_action(&message.public_key, &[expected.clone()]) {
                    Ok(())
                } else {
                    Err(CommitterError::UnauthorizedRole(format!(
                        "{}: action={} allowed={:?} node_roles={:?}",
                        bytes_to_hex(&message.public_key),
                        message.action,
                        expected,
                        roles
                    )))
                }
            }
            AllowedSenders::SelfOnly => {
                if message.public_key != self.sender_public_key() {
                    Err(CommitterError::UnauthorizedRole(format!(
                        "{}: action={} role=SelfOnly",
                        bytes_to_hex(&message.public_key),
                        message.action
                    )))
                } else {
                    Ok(())
                }
            }
            AllowedSenders::AnyRegistered => Ok(()),
        }
    }

    /// This committer's Ed25519 public key — used to gate self-only actions
    /// (e.g. EpochReconcile) against an externally-supplied sender key.
    fn sender_public_key(&self) -> Vec<u8> {
        self.identity.ed25519.public_key().unwrap_or_default()
    }

    /// Get-or-create the per-sender guard used to serialize the commit-time gas
    /// read-modify-write. Returns the owned `Arc<Mutex<()>>` so the caller can hold
    /// its lock only across the `get_user` / subtract / `save_user` sequence, keeping
    /// distinct senders concurrent while two commits for the same sender cannot lose
    /// an update to each other (AUDIT Phase 4.5 / M11). An owned return avoids tying
    /// the returned lock to the brief lifetime of the map lookup.
    async fn rmw_mutex(&self, sender: &[u8]) -> Arc<std::sync::Mutex<()>> {
        let mut cache = self.rmw_locks.lock().await;
        cache
            .entry(sender.to_vec())
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(())))
            .clone()
    }

    /// Build a `CommitterError::GasDeduction` for a failed `get_user`/`save_user`,
    /// and emit a prominent, greppable failure line (sender hex, tx id, gas used,
    /// error) so a silently-free-gas condition is observable in the committer log.
    fn gas_deduction_err(
        &self,
        sender: &[u8],
        tx_id: &str,
        gas_used: u64,
        cause: &DataError,
    ) -> CommitterError {
        self.env_data
            .logger
            .log(format!(
                "GAS DEDUCTION FAILED: sender={} tx_id={} gas_used={} err={:?}",
                bytes_to_hex(sender),
                tx_id,
                gas_used,
                cause
            ));
        CommitterError::GasDeduction {
            sender: bytes_to_hex(sender),
            tx_id: tx_id.to_string(),
            gas_used,
            cause: format!("{cause:?}"),
        }
    }

    /// Build a `CommitterError::SnapshotPersist` for a failed
    /// `save_stake_snapshot`/`save_executor_set`, and emit a prominent,
    /// greppable failure line (epoch, kind, error) so a swallowed snapshot
    /// persistence is observable in the committer log. Surfaces the error
    /// rather than silently advancing with a snapshot that may be missing or
    /// stale (AUDIT Phase 5.4 / H9/M8).
    fn snapshot_save_err(
        &self,
        epoch: u64,
        kind: &'static str,
        cause: &DataError,
    ) -> CommitterError {
        self.env_data
            .logger
            .log(format!(
                "SNAPSHOT PERSIST FAILED: epoch={epoch} kind={kind} err={cause:?}"
            ));
        CommitterError::SnapshotPersist {
            epoch,
            kind,
            cause: format!("{cause:?}"),
        }
    }

    /// Build a `CommitterError::TokenConflict` for a token distribution that carries an id already
    /// present in the local token cache (AUDIT Phase 5.5 / H13), and emit a prominent, greppable
    /// rejection line so a token-swap attempt is observable in the committer log rather than silent.
    fn token_distribution_conflict_err(&self, token_id: &[u8]) -> CommitterError {
        let token_id_hex = bytes_to_hex(token_id);
        self.env_data.logger.log(format!(
            "TOKEN REPLACEMENT REJECTED: token_id={} already present — refusing token swap (AUDIT Phase 5.5 / H13)",
            token_id_hex
        ));
        CommitterError::TokenConflict(token_id_hex)
    }


    // -----------------------------------------------------------------------
    // Utilities (token cache + epoch accessor)
    // -----------------------------------------------------------------------

    /// Add a token to the local cache (for bootstrapping).
    pub fn bootstrap_token(&self, token: Token) {
        self.tokens.insert(token.id.clone(), token);
    }

    /// Get the number of cached tokens.
    pub fn cached_token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Get a copy of a cached token by its ID.
    pub fn get_token(&self, token_id: &[u8]) -> Option<Token> {
        self.tokens.get(token_id).map(|entry| entry.value().clone())
    }

    /// Get the proposal poll interval in milliseconds.
    pub fn proposal_interval_ms(&self) -> u64 {
        self.proposal_interval_ms
    }

    /// Get the current epoch number. Surfaced so the off-thread registration
    /// stake cache (`StakeIndex`) can advance its refresher to the current
    /// epoch on epoch boundaries (AUDIT Phase 4.4) — the single source of
    /// truth for "which epoch's stake set is live" is this counter.
    pub fn current_epoch_number(&self) -> u64 {
        self.current_epoch_number.load(Ordering::SeqCst)
    }

    /// Get a reference to the logger.
    pub fn logger(&self) -> &Arc<dyn Logger> {
        &self.env_data.logger
    }
}


// The per-domain test modules (gas, commit, conflict, finality, epoch, quorum,
// distribution) and the shared `helpers` module now live under src/committer/ as
// child modules of `tests`. Because they are descendants of `crate::committer`,
// they retain access to the Committer's private fields with no accessor changes.
#[cfg(test)]
mod tests {
    pub mod helpers;
    mod gas;
    mod commit;
    mod conflict;
    mod finality;
    mod epoch;
    mod quorum;
    mod distribution;
    mod pool;
}
