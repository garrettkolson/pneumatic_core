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
enum CommitConflictOutcome {
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

impl Committer {
    /// Create a new Committer with all required components.
    #[allow(clippy::too_many_arguments)]
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
    // Commit message handling
    // -----------------------------------------------------------------------

    /// Handle a "Commit" message — the core pipeline step.
    ///
    /// Flow:
    /// 1. Deserialize the TransactionCommit from the message body
    /// 2. Validate the transaction message (env_id, block hash)
    /// 3. Acquire lock and verify Finalizing state
    /// 4. Commit the block via BlockServices
    /// 5. Transition to Committed, release lock
    async fn handle_commit(&self, message: Message) -> Result<(), CommitterError> {
        // Deserialize the TransactionCommit from the message body
        let commit: TransactionCommit =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        // Validate the transaction message
        self.validate_transaction_message(&commit)?;

        // Check and commit the transaction results. The authenticated sender key (verified by
        // `authenticate_message`) is threaded through as the finalizer key: it identifies the
        // node that actually authenticated this Commit, which is more trustworthy than the
        // `finalizer_addr` self-declared inside the wire block.
        self.check_and_commit_transaction_results(&commit, message.public_key.clone()).await
    }

    /// Check and commit transaction results.
    ///
    /// Accepts transactions in two states:
    /// - **Finalizing**: standard pipeline (Sentinel → Executor → Finalizer → Committer).
    ///   The transaction was assigned a finalizer and collected quorum signatures.
    /// - **Validated**: leader-proposal path. The leader dequeued the transaction
    ///   from the pool and proposed it directly. No finalizer key is present.
    ///
    /// Flow:
    /// 1. Acquire lock on the transaction in the pending registry
    /// 2. Verify the transaction is in Finalizing OR Validated state
    /// 3. Apply the block via BlockServices (commit + distribute)
    /// 4. Update transaction state to Committed (or remove from pool for leader path)
    /// 5. Release the transaction lock
    async fn check_and_commit_transaction_results(
        &self,
        commit: &TransactionCommit,
        finalizer_key: Vec<u8>,
    ) -> Result<(), CommitterError> {
        let tx_id = String::from_utf8_lossy(&commit.trans_id).to_string();

        // AUDIT Phase 4.1 / H4: sink path. In the live pipeline the pending registry is normally
        // populated upstream (e.g. by the sentinel), so this entry usually already exists here. But
        // a Commit may arrive with no registry entry — e.g. a self-contained Finalizer→Committer
        // flow, or an empty registry at boot. Rather than fail closed on `TransactionNotInFinalizing`
        // for a transaction that is otherwise authentic (envelope-verified, registered sender,
        // valid finalizer signature on the block), materialize it here as `Finalizing` from the wire
        // block's transaction, keyed to the authenticated finalizer. The H12 hash check below then
        // binds this transaction to the committed payload.
        if !self.pending_registry.contains(&tx_id) {
            let entry = PendingTransaction::new(
                tx_id.clone(),
                TransactionState::Finalizing {
                    transaction: commit.proposed_block.signed_trans.transaction.clone(),
                    finalizer_key: finalizer_key.clone(),
                },
            );
            self.pending_registry
                .add_transaction(tx_id.clone(), entry)
                .map_err(|_| CommitterError::TransactionNotInFinalizing(tx_id.clone()))?;
        }

        // Step 1: Acquire lock on the transaction
        self.pending_registry
            .acquire_transaction(&tx_id)
            .map_err(|_| CommitterError::TransactionNotInFinalizing(tx_id.clone()))?;

        // Step 2: Extract transaction from either Finalizing (standard pipeline)
        // or Validated (leader-proposal) state.
        let (transaction, is_leader_proposal) = {
            let entry = self
                .pending_registry
                .get_transaction_mut(&tx_id)?;

            match &entry.state {
                TransactionState::Finalizing { transaction, .. } => {
                    (transaction.clone(), false)
                }
                TransactionState::Validated { transaction, .. } => {
                    (transaction.clone(), true)
                }
                _ => {
                    return Err(CommitterError::TransactionNotInFinalizing(tx_id));
                }
            }
        };

        // AUDIT Phase 3.5 / H12: commit the validated payload, not whatever arrived. The wire
        // TransactionCommit carries its own `proposed_block`; that block is what actually gets
        // appended to the chain, yet nothing above verifies its embedded transaction is the one we
        // validated and pooled. Hash-compare the wire block's transaction against the validated one
        // (a full-payload hash, so a swap of any field is caught) — fail closed on any mismatch.
        if transaction.hash()? != commit.proposed_block.signed_trans.transaction.hash()? {
            return Err(CommitterError::TransactionPayloadMismatch(tx_id.clone()));
        }

        // Step 3: Check for conflicts and resolve before committing. The resolved
        // conflict tells us whether the incoming block wins outright (`Commit`) or
        // wins only after the losing tip is rolled back (`CommitWinnerAfterRollback`)
        // — or was rejected (`LoserDiscarded`, surfaced below).
        match self.handle_conflict_at_commit(commit, &finalizer_key) {
            Ok(CommitConflictOutcome::Commit) => {
                self.block_services.commit_block(commit, None)?;
            }
            Ok(CommitConflictOutcome::CommitWinnerAfterRollback(loser_hash)) => {
                self.block_services.commit_block(commit, Some(loser_hash))?;
            }
            Err(e) => return Err(e),
        }

        // Step 3.5: Deduct gas from sender's fuel balance.
        //
        // AUDIT Phase 4.5 / M11: fail-closed. Any failure to read or persist the
        // sender's balance is surfaced (a loud log line + a returned
        // `CommitterError::GasDeduction`) rather than silently swallowed, so gas is
        // never given for free. The per-sender lock serializes the read-modify-write so
        // concurrent commits for the same sender cannot lose an update. The `if let
        // Some` guard is deliberate: a tx with no tracked gas (never routed through the
        // cost model) is not debited and is not an error. The partition is the
        // environment's token partition (the same one `verify_gas` debited at admission).
        if let Some(gas_used) = self.pending_registry.get_gas_used(&tx_id) {
            let mutex = self.rmw_mutex(&transaction.sender).await;
            let _guard = mutex.lock().unwrap_or_else(|p| p.into_inner());
            let user = self
                .data_provider
                .get_user(&transaction.sender, &self.env_data.token_partition_id)
                .map_err(|e| self.gas_deduction_err(&transaction.sender, &tx_id, gas_used, &e))?;
            let mut user = user;
            user.fuel_balance = user.fuel_balance.saturating_sub(gas_used);
            self.data_provider.save_user(
                &transaction.sender,
                user,
                &self.env_data.token_partition_id,
            )
            .map_err(|e| self.gas_deduction_err(&transaction.sender, &tx_id, gas_used, &e))?;
        }

        // Step 4: Update transaction state
        if is_leader_proposal {
            // Leader-proposal path: remove from pool, then transition to Committed
            // so that release() returns true (entry cleanup requires Committed/Failed state).
            self.pending_registry.remove_from_pool(&tx_id);
        }
        // Transition to Committed for BOTH paths — release() checks for Committed/Failed
        // to decide whether to remove the entry when lock_count reaches 0.
        if let Ok(mut entry) = self.pending_registry.get_transaction_mut(&tx_id) {
            // AUDIT Phase 3.5 / H12: `block_hash` holds the hash of the block the transaction was
            // committed *into* — here the committed block's own hash (the finalizer already stores
            // this). Previously `result.token_id` (a token id) was stored, a misnomer.
            entry.transition_to_committed(transaction, commit.proposed_block.current_hash.clone());
        }

        // Step 5: Release the transaction lock
        let should_remove = self.pending_registry.release_transaction(&tx_id)?;

        if should_remove {
            let _ = self.pending_registry.remove_transaction(&tx_id);
        }

        // Step 6: Distribute the committed block to archivers
        let _ = self.block_services.distribute_to_archivers(&commit.proposed_block).await;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Token distribution handling
    // -----------------------------------------------------------------------

    /// Handle token distribution from other committers.
    ///
    /// Inserts a freshly-distributed token into the local cache for future commits. A token IS its
    /// own blockchain — its chain and metadata are authoritative, so a peer may not swap in an
    /// alternative under an id that already exists (AUDIT Phase 5.5 / H13). A distribution carrying
    /// a new id is accepted (this is how a node joining the network seeds a token it lacks); a
    /// distribution whose id is already cached is refused and the cached token is left intact.
    async fn handle_token_distribution(&self, message: Message) -> Result<(), CommitterError> {
        let token: Token = deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        // Reject-on-conflict: `entry()` atomically checks-and-inserts under a single shard write
        // guard, the same single-operation shape as `handle_block_finalized`'s `get_mut`
        // (AUDIT Phase 3.3 / C5). A `contains_key`-then-`insert` would leave a read-then-write gap
        // where two concurrent distributions for a not-yet-existing id could both pass the check and
        // one would silently overwrite the other — the same swap vector we are closing here.
        match self.tokens.entry(token.id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(token);
                Ok(())
            }
            Entry::Occupied(_) => Err(self.token_distribution_conflict_err(&token.id)),
        }
    }

    // -----------------------------------------------------------------------
    // Block distribution handling
    // -----------------------------------------------------------------------

    /// Handle block distribution from other committers.
    /// Logs receipt for observability.
    async fn handle_block_distribution(&self, message: Message) -> Result<(), CommitterError> {
        let block: pneumatic_core::blocks::Block =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        let logger = &self.env_data.logger;
        logger.log(format!(
            "Received distributed block (hash: {})",
            bytes_to_hex(&block.current_hash)
        ));

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Block quorum gossip handling
    // -----------------------------------------------------------------------

    /// Handle "BlockFinalized" gossip from the finalizer.
    ///
    /// This is the entry point for quorum gossip — the finalizer has
    /// committed a block and is broadcasting it to all nodes.
    /// Receivers validate the block, append it, and broadcast a vote.
    /// Fail-closed finalizer-signature check (AUDIT Phase 3.3 / C5).
    ///
    /// A `BlockFinalized` block carries exactly one authoritative signature — the finalizer's, in
    /// `SignedTransaction.finalizer_sig`. Its `transaction_hash` is the hash the finalizer actually
    /// signed, so the Ed25519 verification needs no reconstruction. Because `create_hash` binds the
    /// whole `finalizer_sig` into the block hash (canonical serialization includes it), a swapped
    /// signature would already fail the linkage/hash check in `append_validated_block`. Reject a
    /// missing or unverified signature rather than silently accepting it.
    fn verify_block_finalizer_sig(&self, block: &Block) -> Result<(), CommitterError> {
        let finalizer_sig = &block.signed_trans.finalizer_sig;
        if finalizer_sig.signature.is_empty() {
            return Err(CommitterError::InvalidFinalizerSignature);
        }
        let valid = self.identity.ed25519.check_signature(
            &finalizer_sig.signature,
            &block.signed_trans.finalizer_addr,
            &finalizer_sig.transaction_hash,
        )?;
        if !valid {
            return Err(CommitterError::InvalidFinalizerSignature);
        }
        Ok(())
    }

    async fn handle_block_finalized(&self, message: Message) -> Result<(), CommitterError> {
        let block: Block = deserialize_rmp_to(&message.body)
            .map_err(CommitterError::Deserialization)?;

        let block_hash = block.current_hash.clone();
        let token_id = &block.signed_trans.transaction.token_id;

        // Cache the stake set for later vote processing
        if let Some(ref stake_set) = message.stake_set {
            let mut cache = self.stake_set_cache.lock().await;
            cache.insert(block_hash.clone(), stake_set.clone());
        }

        // Fail-closed finalizer-signature check (AUDIT Phase 3.3 / C5): reject a block whose
        // finalizer signature does not verify. Pure computation over the block's own fields — no
        // lock is held here.
        self.verify_block_finalizer_sig(&block)?;

        // Validate linkage + hash and append under a SINGLE mutable borrow of the token (AUDIT
        // Phase 3.3 / C5). This closes the read-then-`get_mut` gap: previously the tip was read via
        // an immutable borrow, dropped, and only then re-looked-up mutably — so two concurrent
        // sibling blocks could both validate and both append. `append_validated_block` reads the tip
        // and appends inside one `&mut self`, which maps here to a single `get_mut` on the token.
        // Blocks committed by this call — the original append plus any promoted from the orphan
        // buffer. Each is distributed to archivars and voted on, exactly as in the plain path.
        let mut committed: Vec<Block> = Vec::new();

        // Validate linkage + hash and append under a SINGLE mutable borrow of the token (AUDIT
        // Phase 3.3 / C5). This closes the read-then-`get_mut` gap: previously the tip was read via
        // an immutable borrow, dropped, and only then re-looked-up mutably — so two concurrent
        // sibling blocks could both validate and both append. `append_validated_block` reads the tip
        // and appends inside one `&mut self`, which maps here to a single `get_mut` on the token.
        {
            let mut entry = self.tokens.get_mut(token_id).ok_or_else(|| {
                CommitterError::TokenNotFound(bytes_to_hex(token_id))
            })?;
            match entry.value_mut().blockchain.append_validated_block(&block) {
                AppendOutcome::LinkageMismatch => {
                    // AUDIT Phase 3.4 / H15: the receiver is behind — this is the next block in a
                    // sequence whose parent has not yet landed, NOT a sibling competitor. Buffer it
                    // and replay it as the tip advances instead of silently dropping it.
                    self.buffer_orphan(token_id.clone(), block).await;
                    return Ok(());
                }
                AppendOutcome::InvalidHash => {
                    self.env_data.logger.log(format!(
                        "BlockFinalized: invalid hash for token [{}], rejecting",
                        bytes_to_hex(token_id)
                    ));
                    return Err(CommitterError::InvalidBlockHash);
                }
                AppendOutcome::Appended => {
                    committed.push(block.clone());
                }
            }
        }

        // AUDIT Phase 3.4 / H15: the tip just advanced by `block_hash`. Replay any buffered blocks
        // whose parent is now the tip, cascading as the promoted chain grows.
        let promoted = self.replay_orphan_blocks(&block_hash, token_id).await;
        committed.extend(promoted);

        // Propagate every block we committed this call (the original plus the promoted ones).
        for committed_block in &committed {
            // Distribute to archivars (propagate gossip)
            let _ = self.block_services.distribute_to_archivers(committed_block).await;

            // Broadcast our own vote: we've received and validated this block
            self.broadcast_vote(&committed_block.current_hash).await;
        }

        Ok(())
    }

    /// Buffer an out-of-order finalized block for later replay (AUDIT Phase 3.4 / H15).
    ///
    /// Called when a BlockFinalized's block does not chain onto the current tip. The block is held
    /// in the orphan buffer keyed by token; whether it was buffered — or dropped because the buffer
    /// is full — is always logged, so the drop is observable, never silent.
    async fn buffer_orphan(&self, token_id: Vec<u8>, block: Block) {
        let mut orphan_blocks = self.orphan_blocks.lock().await;
        // Compute the token hex before `token_id` is moved into `insert` below.
        let token_hex = bytes_to_hex(&token_id);
        match orphan_blocks.insert(token_id, block) {
            BufferDecision::Buffered => {
                self.env_data.logger.log(format!(
                    "BlockFinalized: buffered out-of-order block for token [{token_hex}] for replay"
                ));
            }
            BufferDecision::RejectedFull => {
                self.env_data.logger.log(format!(
                    "BlockFinalized: orphan buffer full for token [{token_hex}], dropping out-of-order block"
                ));
            }
        }
    }

    /// Promote buffered blocks whose parent hash is `tip_hash`, cascading to blocks whose parent
    /// chains onto each promoted block (AUDIT Phase 3.4 / H15).
    ///
    /// A candidate is selected under the orphan lock only, then appended under a single `get_mut`
    /// on the token (the same atomic read-tip-then-append shape as the plain path), so a promoted
    /// append cannot race a concurrent handler and there is no nested lock. Returns the promoted
    /// blocks in commit order.
    async fn replay_orphan_blocks(
        &self,
        tip_hash: &[u8],
        token_id: &[u8],
    ) -> Vec<Block> {
        let mut committed = Vec::new();
        let mut expected_tip = tip_hash.to_vec();

        loop {
            // Select the next buffered block that chains onto `expected_tip`, removing it from the
            // buffer so it is "in flight" even if eviction or a concurrent handler touches it next.
            let chosen = {
                let mut orphan_blocks = self.orphan_blocks.lock().await;
                orphan_blocks.drop_expired(Instant::now());
                orphan_blocks.take_matching(token_id, &expected_tip, Instant::now())
            };

            let chosen = match chosen {
                Some(chosen) => chosen,
                None => break,
            };

            // Append atomically. The orphan lock is released before the token lock is taken, so
            // there is no new lock-ordering hazard.
            let append_outcome = {
                match self.tokens.get_mut(token_id) {
                    Some(mut entry) => {
                        entry.value_mut().blockchain.append_validated_block(&chosen)
                    }
                    None => break,
                }
            };

            match append_outcome {
                AppendOutcome::Appended => {
                    committed.push(chosen.clone());
                    // The promoted block's own hash is now the tip — keep cascading.
                    expected_tip = chosen.current_hash.clone();
                }
                AppendOutcome::InvalidHash => {
                    // A buffered block that is now internally inconsistent (tampered) — fail
                    // closed and stop the cascade.
                    self.env_data.logger.log(format!(
                        "BlockFinalized: replayed block for token [{}] failed hash check, rejecting",
                        bytes_to_hex(token_id)
                    ));
                    break;
                }
                AppendOutcome::LinkageMismatch => {
                    // A sibling raced in and advanced the tip past `chosen`'s parent while we
                    // were selecting. Re-queue it so a later replay can retry if the tip returns.
                    let mut orphan_blocks = self.orphan_blocks.lock().await;
                    orphan_blocks.requeue_back(token_id, chosen);
                    break;
                }
            }
        }

        committed
    }

    /// Handle "BlockConfirmed" vote from peers.
    ///
    /// Each node that validates a BlockFinalized message broadcasts a vote.
    /// This handler accumulates votes and checks for quorum.
    async fn handle_block_confirmed_vote(&self, message: Message) -> Result<(), CommitterError> {
        // Deserialize vote: (block_hash, voter_public_key)
        let (block_hash, voter_key): (Vec<u8>, Vec<u8>) =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        // Look up stake set for this block
        let cumulative_stake = {
            let cache = self.stake_set_cache.lock().await;
            let stake_set = match cache.get(&block_hash) {
                Some(ss) => ss,
                None => {
                    // Vote received before BlockFinalized — ignore
                    return Ok(());
                }
            };

            let voting_stake = stake_set.get_stake(&voter_key);

            // Skip our own vote (we already voted via handle_block_finalized)
            if voter_key == self.public_key {
                return Ok(());
            }

            // Accumulate vote
            let mut votes = self.confirmation_votes.lock().await;
            let entry = votes.entry(block_hash.clone()).or_insert_with(|| {
                (HashSet::new(), 0u64)
            });

            // Count stake only once per key
            if !entry.0.insert(voter_key) {
                // Already counted
                return Ok(());
            }

            entry.1 = entry.1.saturating_add(voting_stake);
            entry.1
        };

        // Check if quorum reached
        if cumulative_stake > 0 {
            let total_stake = {
                let cache = self.stake_set_cache.lock().await;
                cache.get(&block_hash).map(|ss| ss.total_stake())
            };

            if let Some(total) = total_stake {
                let quorum_threshold = total as f64 * (self.env_data.quorum_percentage as f64) / 100.0;
                if cumulative_stake as f64 >= quorum_threshold {
                    // Quorum reached — broadcast status to all peers
                    let _ = self.broadcast_quorum_reached(&block_hash).await;
                }
            }
        }

        Ok(())
    }

    /// Handle "BlockQuorumReached" status update.
    ///
    /// All nodes that receive this transition the block to Confirmed.
    /// No further quorum check needed — the broadcaster verified quorum.
    async fn handle_block_quorum_reached(&self, message: Message) -> Result<(), CommitterError> {
        let block_hash: Vec<u8> =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        // Find matching token id without holding a read lock across mutation.
        let mut matched_token_id: Option<Vec<u8>> = None;
        for token_entry in self.tokens.iter() {
            let token = token_entry.value();
            let count = token.blockchain.get_count();
            if count == 0 {
                continue;
            }
            if let Some(tip) = token.blockchain.get_block_at(count - 1) {
                if tip.current_hash == block_hash {
                    matched_token_id = Some(token.id.clone());
                    break;
                }
            }
        }

        if let Some(token_id) = matched_token_id {
            let mut entry = self.tokens.get_mut(&token_id).ok_or_else(|| {
                CommitterError::TokenNotFound(bytes_to_hex(&token_id))
            })?;
            let _ = entry.value_mut().blockchain.set_finality_status(&block_hash, FinalityStatus::Confirmed);
        }

        Ok(())
    }

    /// Broadcast BlockQuorumReached to all peer node types.
    async fn broadcast_quorum_reached(&self, block_hash: &[u8]) -> Result<(), CommitterError> {
        let body = serialize_to_bytes_rmp(&block_hash.to_vec())
            .map_err(|_| CommitterError::InternalSerialization)?;

        let message = Message::signed(
            self.env_data.environment_id.clone(),
            "BlockQuorumReached",
            body,
            None,
            &self.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&message)
            .map_err(|_| CommitterError::InternalSerialization)?;

        // Broadcast to all other node types
        let _ = self.node_registry.send_to_all(payload.clone(), &NodeRegistryType::Committer).await;
        let _ = self.node_registry.send_to_all(payload.clone(), &NodeRegistryType::Archiver).await;
        let _ = self.node_registry.send_to_all(payload.clone(), &NodeRegistryType::Executor).await;
        let _ = self.node_registry.send_to_all(payload, &NodeRegistryType::Sentinel).await;

        Ok(())
    }

    /// Broadcast a BlockConfirmed vote from this committer.
    /// Called after handle_block_finalized to vote on a block.
    async fn broadcast_vote(&self, block_hash: &[u8]) {
        // Serialize vote: (block_hash, this node's public key)
        let body = serialize_to_bytes_rmp(&(block_hash.to_vec(), self.public_key.clone()))
            .map_err(|_| ());

        let message = Message::signed(
            self.env_data.environment_id.clone(),
            "BlockConfirmed",
            body.unwrap_or_default(),
            None,
            &self.identity,
        )
        .map_err(|_| ());

        let payload = message
            .and_then(|m| serialize_to_bytes_rmp(&m).map_err(|_| ()));

        if let Ok(p) = payload {
            let _ = self.node_registry.send_to_all(p.clone(), &NodeRegistryType::Committer).await;
            let _ = self.node_registry.send_to_all(p.clone(), &NodeRegistryType::Archiver).await;
            let _ = self.node_registry.send_to_all(p.clone(), &NodeRegistryType::Executor).await;
            let _ = self.node_registry.send_to_all(p, &NodeRegistryType::Sentinel).await;
        }
    }

    // -----------------------------------------------------------------------
    // Epoch reconciliation handling
    // -----------------------------------------------------------------------

    /// Handle epoch reconciliation request.
    ///
    /// Flow:
    /// 1. Run the epoch reconciler to analyze chain state
    /// 2. Apply staking operations from reconciliation
    /// 3. Select new leader for the epoch
    async fn handle_epoch_reconcile(&self) -> Result<(), CommitterError> {
        // Run reconciliation
        let reconciliation = self.epoch_reconciler.reconcile();

        if !reconciliation.slashing_ops.is_empty() || !reconciliation.reward_ops.is_empty() {
            // Apply staking operations
            self.staking_manager.apply_ops(&reconciliation)?;
        }

        // Advance to the next epoch via the single guarded writer (AUDIT Phase 5.4
        // / H9/M8). `advance_epoch_to` binds the leader seed to the mined tip,
        // advances the detector (the authoritative epoch source), mirrors the new
        // number into the atomic counter, and persists the stake + executor
        // snapshots — so the wire path can never diverge from or rewind the
        // counter, and a snapshot persistence failure aborts the advance.
        let advanced = self.advance_epoch_to()?;

        // Log the newly selected leader if the advance produced one.
        if let Some(leader_key) = advanced {
            let logger = &self.env_data.logger;
            if !leader_key.is_empty() {
                logger.log(format!(
                    "Epoch leader selected: {}",
                    bytes_to_hex(&leader_key)
                ));
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Conflict detection and resolution
    // -----------------------------------------------------------------------

    /// Check for and resolve block conflicts at commit time.
    ///
    /// Before committing a block, check the CandidateRegistry for competing
    /// proposals at the same (token_id, previous_hash). If a conflict is
    /// detected, resolve it using stake-weighted selection with proposer
    /// identity branching:
    /// - Different proposers → DiscardLoser (network race)
    /// - Same proposer → SameProposerSlash (double-signed)
    /// - Equal stakes + different proposers → TieFlagBoth (fail-closed; the old
    ///   equal-stake hash tie-break was removed as attacker-grindable — Phase 5.8)
    ///
    /// Every resolved group is then cleared from the registry via
    /// `remove_conflicted`, and the loser is never left standing (AUDIT Phase
    /// 5.2 / H2): the incoming block commits only if it wins — appended as-is
    /// (outcome `Commit`) or, if the loser happens to be the current tip, after
    /// that tip is rolled back (outcome `CommitWinnerAfterRollback`). Any other
    /// resolution rejects the incoming block with `Err(CommitterError::LoserDiscarded)`.
    fn handle_conflict_at_commit(
        &self,
        commit: &TransactionCommit,
        verified_proposer: &[u8],
    ) -> Result<CommitConflictOutcome, CommitterError> {
        let token_id = commit.token_id.clone();
        let previous_hash = commit.proposed_block.previous_hash.clone();
        let incoming_hash = commit.proposed_block.current_hash.clone();

        // Check for existing candidates at this position
        let candidates = self.candidate_registry
            .get_candidates(&token_id, &previous_hash);

        if candidates.is_empty() {
            // No conflict — this is the first candidate for this position.
            // Insert it into the registry for future conflict detection. The
            // incoming block is the winner by default (no rollback needed).
            //
            // Record the *verified* proposer (the authenticated envelope sender,
            // `message.public_key`) — never the self-declared `proposer_key`, which
            // is unsigned and could be forged to inflate stake or mis-trigger a
            // slash in a later conflict resolution. (AUDIT Phase 5.8 / M10)
            self.candidate_registry.insert(
                token_id, previous_hash,
                commit.proposed_block.clone(),
                verified_proposer.to_vec(),
            );
            return Ok(CommitConflictOutcome::Commit);
        }

        // Conflict detected — fold the incoming block over ALL candidates (AUDIT
        // Phase 5.8 / M10): the incoming wins only if it beats every candidate, and
        // losing to any one rejects it. Build the StakeSet once for resolution.
        let stake_set = StakeSet {
            stakers: self.stake_store.iter()
                .map(|(k, s)| (k.clone(), s))
                .collect(),
        };

        // Reject the incoming block if it loses to ANY candidate, or if any pair is a
        // SameProposerSlash or TieFlagBoth (fail-closed). Resolution uses the verified
        // proposer, never the self-declared field.
        let mut rollback_target = incoming_hash.clone();
        for (candidate, candidate_proposer) in &candidates {
            let candidate_hash = candidate.current_hash.clone();

            match resolve_block_conflict(
                &incoming_hash, &candidate_hash,
                verified_proposer, candidate_proposer,
                &stake_set,
            ).map_err(|e| CommitterError::Core(e))? {
                // Incoming loses to this candidate (winner is the candidate) — reject it.
                ConflictResolution::DiscardLoser(winner_hash) if winner_hash != incoming_hash => {
                    self.env_data.logger.log(format!(
                        "Conflict resolved (DiscardLoser) at commit: candidate {} beats incoming {} (token: {})",
                        bytes_to_hex(&candidate_hash), bytes_to_hex(&incoming_hash), bytes_to_hex(&token_id),
                    ));
                    self.candidate_registry.remove_conflicted(&token_id, &previous_hash);
                    return Err(CommitterError::LoserDiscarded);
                }
                // Incoming wins vs this candidate — record the buffered loser as the rollback
                // target and continue folding over the rest.
                ConflictResolution::DiscardLoser(_) => rollback_target = candidate_hash,
                // Same verified proposer double-signed — slash and reject incoming.
                ConflictResolution::SameProposerSlash(_, slashed_key) => {
                    let amount = (self.stake_store.get_stake(&slashed_key) as f64
                        * self.env_data.cost_model.slash_fraction)
                        .round()
                        .min(u64::MAX as f64) as u64;

                    self.env_data.logger.log(format!(
                        "Double-proposal detected (same proposer) at commit: slashing {} (candidate {} vs incoming {})(token: {})",
                        bytes_to_hex(&slashed_key), bytes_to_hex(&candidate_hash), bytes_to_hex(&incoming_hash), bytes_to_hex(&token_id),
                    ));
                    self.staking_manager.apply_ops(&pneumatic_core::epoch::EpochReconciliation {
                        misshapen_tokens: vec![],
                        finalization_conflicts: vec![],
                        slashing_ops: vec![pneumatic_core::epoch::StakingOp::Slash(
                            slashed_key, amount,
                        )],
                        reward_ops: vec![],
                    })?;

                    self.candidate_registry.remove_conflicted(&token_id, &previous_hash);
                    return Err(CommitterError::LoserDiscarded);
                }
                // Equal stakes + different verified proposers — fail closed: flag both
                // for review and reject the incoming block.
                ConflictResolution::TieFlagBoth(_) => {
                    self.env_data.logger.log(format!(
                        "Tie conflict at commit — equal stakes + different verified proposers, flagging both for review and rejecting incoming (token: {})",
                        bytes_to_hex(&token_id),
                    ));
                    self.candidate_registry.remove_conflicted(&token_id, &previous_hash);
                    return Err(CommitterError::LoserDiscarded);
                }
            }
        }

        // Incoming beats every candidate — commit it. rollback_target is the buffered loser's
        // hash (the tip that the incoming displaces). commit_block rolls back only when that
        // target matches the current tip — a buffered candidate that is not on the chain leaves
        // the tip untouched, so the incoming appends to the real tip. (AUDIT Phase 5.2 / H2,
        // 5.8 / M10: the rollback target is the loser, never the winner's own hash.)
        self.candidate_registry.remove_conflicted(&token_id, &previous_hash);
        Ok(CommitConflictOutcome::CommitWinnerAfterRollback(rollback_target))
    }

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    /// Validate a Commit message.
    ///
    /// Checks:
    /// 1. The commit's env_id matches this committer's environment
    /// 2. The proposed block's hash is non-empty
    fn validate_transaction_message(&self, commit: &TransactionCommit) -> Result<(), CommitterError> {
        // Environment ID check
        if commit.env_id != self.env_data.environment_id {
            return Err(CommitterError::EnvironmentMismatch {
                expected: self.env_data.environment_id.clone(),
                got: commit.env_id.clone(),
            });
        }

        // Block hash check
        if commit.proposed_block.current_hash.is_empty() {
            return Err(CommitterError::InvalidBlockHash);
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Utilities
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

    // -----------------------------------------------------------------------
    // Block proposal
    // -----------------------------------------------------------------------

    /// Advance to a new epoch: bump the epoch number, select a new leader,
    /// and save the previous leader for stale block detection.
    ///
    /// Thin wrapper over `advance_epoch_to`, the single guarded epoch advance
    /// shared by this internal path and the wire `EpochReconcile` path (AUDIT
    /// Phase 5.4 / H9/M8). Returns the new leader on a real advance, `None` when
    /// the advance was rejected (same/older epoch number, or the detector lock was
    /// already held), and `Err(CommitterError::SnapshotPersist)` if a snapshot
    /// fails to persist.
    fn advance_epoch(&self) -> Result<Option<Vec<u8>>, CommitterError> {
        self.advance_epoch_to()
    }

    /// The one writer for the epoch number (AUDIT Phase 5.4 / H9/M8).
    ///
    /// The `EpochBoundaryDetector`'s epoch is the authoritative source of truth.
    /// Both the internal production path (`advance_epoch`) and the wire
    /// `EpochReconcile` path (`handle_epoch_reconcile`) funnel here, so they can
    /// never disagree on or rewind the epoch number, and the counter can never
    /// fall behind the detector.
    ///
    /// Returns:
    ///  - `Ok(Some(new_leader))` on a real advance (detector advanced, number
    ///    mirrored, snapshots persisted);
    ///  - `Ok(None)` when the advance is rejected — the target epoch is not
    ///    strictly greater than the authoritative value (a reused/rewinding
    ///    number), or the detector lock is already held by another writer;
    ///  - `Err(CommitterError::SnapshotPersist{...})` if a snapshot fails to
    ///    persist, so the advance aborts rather than proceeding on a possibly
    ///    stale snapshot.
    fn advance_epoch_to(&self) -> Result<Option<Vec<u8>>, CommitterError> {
        let stake_set = self.stake_store.to_stake_set();
        // Phase 5.3 / H3: bind the new leader seed to the mined chain tip so the
        // epoch's leader is only knowable once this tip is produced. Read the
        // current tip from the local token cache — the committer holds its chain
        // state there and does not persist it to the data service.
        let prev_block_hash = self
            .tokens
            .iter()
            .map(|entry| entry.value().blockchain.get_current_chain_state().last_hash_in)
            .next()
            .unwrap_or_default();

        // The detector lock serializes the two writers: the internal path and the
        // wire path cannot both be mid-advance, and it is released before
        // propose_blocks re-locks it for is_epoch_expired. try_lock fails closed —
        // if it is already held, treat the advance as a no-op rather than block.
        let mut detector = self
            .epoch_detector
            .try_lock()
            .map_err(|_| CommitterError::InternalSerialization)?;
        let detector = detector
            .as_mut()
            .ok_or(CommitterError::InternalSerialization)?;

        // The detector's epoch is authoritative; the mirrored counter must track
        // it. Reject an advance whose target does not strictly exceed both.
        let current_epoch_number = detector.current_epoch.epoch_number;
        let new_epoch_number = current_epoch_number + 1;
        let stored = self.current_epoch_number.load(Ordering::SeqCst);
        if stored >= new_epoch_number {
            // Either the counter has already advanced past this target (a reused
            // number) or it is ahead of the detector (the divergence this funnel
            // removes) — neither is a valid advance, so rewind is refused.
            self.env_data
                .logger
                .log(format!(
                    "REJECT EPOCH ADVANCE: epoch {} would not exceed stored {} (detector epoch {})",
                    new_epoch_number, stored, current_epoch_number
                ));
            return Ok(None);
        }

        detector.advance_to_new_epoch(
            self.leader_selector.as_ref(),
            &stake_set,
            self.epoch_duration,
            &prev_block_hash,
        );
        let new_leader = detector.current_epoch.leader_public_key.clone();
        // Mirror the authoritative epoch into the atomic counter — the two now
        // move together, so the counter can never lag the detector again.
        self.current_epoch_number.store(new_epoch_number, Ordering::SeqCst);

        // Persist both snapshots, surfacing (not swallowing) any persistence
        // failure so a stale or missing snapshot never silently enters the pipeline.
        self.data_provider
            .save_stake_snapshot(
                new_epoch_number,
                stake_set.clone(),
                &self.env_data.token_partition_id,
            )
            .map_err(|e| self.snapshot_save_err(new_epoch_number, "stake", &e))?;
        self.data_provider
            .save_executor_set(
                new_epoch_number,
                stake_set.to_executor_set(),
                &self.env_data.token_partition_id,
            )
            .map_err(|e| self.snapshot_save_err(new_epoch_number, "executor", &e))?;

        Ok(Some(new_leader))
    }

    /// Propose a batch of transactions for a given token.
    ///
    /// Steps:
    /// 1. Check epoch expiry — if expired, advance to new epoch
    /// 2. Verify this node is the current epoch leader
    /// 3. Dequeue transactions from the pool via BlockProposer
    /// 4. Build TransactionCommit for each dequeued transaction
    /// 5. Return commits for dispatch to the Finalizer
    pub async fn propose_blocks(
        &self,
        token_id: &[u8],
        limit: usize,
    ) -> Result<Vec<TransactionCommit>, CommitterError> {
        // Step 1: Check epoch expiry and advance if needed
        {
            let now = chrono::Utc::now().timestamp();
            let should_advance = {
                let detector = self.epoch_detector.lock().await;
                if let Some(ref d) = *detector {
                    d.is_epoch_expired(now)
                } else {
                    false
                }
            };

            if should_advance {
                match self.advance_epoch() {
                    Ok(Some(new_leader)) => {
                        let epoch_num = self.current_epoch_number.load(Ordering::SeqCst);
                        let logger = &self.env_data.logger;
                        logger.log(format!(
                            "Epoch advanced to {} (new leader: {}",
                            epoch_num,
                            bytes_to_hex(&new_leader)
                        ));
                    }
                    // Rejected (same/older epoch, or detector lock already held) or
                    // an aborting persistence error — propagate it so propose_blocks
                    // surfaces rather than silently re-runs a stale epoch.
                    Ok(None) => {}
                    Err(e) => return Err(e),
                }
            }
        }

        // Step 2: Check if this node is the current epoch leader
        let is_leader = {
            let detector = self.epoch_detector.lock().await;
            if let Some(ref d) = *detector {
                d.current_leader() == Some(self.public_key.as_slice())
            } else {
                false
            }
        };

        if !is_leader {
            return Ok(Vec::new());
        }

        // Step 3: Dequeue transactions from the pool
        let batch = self
            .block_proposer
            .propose_batch(&self.pending_registry, token_id, limit)
            .map_err(|e| CommitterError::Core(e))?;

        if batch.is_empty() {
            return Ok(Vec::new());
        }

        // Step 4: Build TransactionCommit for each dequeued transaction
        let env_id = self.env_data.environment_id.clone();
        let token_id_vec = token_id.to_vec();
        let mut commits = Vec::with_capacity(batch.len());

        // Resolve the real token and its current chain tip before building blocks
        let token_ref = self.tokens.get(token_id).ok_or_else(|| {
            CommitterError::TokenNotFound(bytes_to_hex(token_id))
        })?;
        let token = token_ref.value();
        let blockchain = token.blockchain.clone();
        let epoch_number = self.current_epoch_number.load(Ordering::SeqCst);

        for (tx, signed) in batch {
            let commit = TransactionCommit {
                trans_id: tx.id.into_bytes(),
                token_id: token_id_vec.clone(),
                env_id: env_id.clone(),
                proposed_block: Block::from_transaction(
                    signed,
                    blockchain.clone(),
                    token,
                    epoch_number,
                ),
            };
            commits.push(commit);
        }

        Ok(commits)
    }

    /// Run the epoch loop: iterate registered token IDs and propose blocks for each.
    pub async fn run_epoch_loop(&self) -> Result<(), CommitterError> {
        // Collect token IDs first: `commit_block` takes a write lock on a token entry
        // (via `self.tokens.get_mut`). Holding the `iter()` read guard across that write
        // would deadlock the shard. Gather the keys up front so no shard lock is held
        // while committing.
        let token_ids: Vec<Vec<u8>> = self.tokens.iter().map(|r| r.key().clone()).collect();
        for token_id in token_ids {
            // AUDIT Phase 4.1 / H4: consume the leader-proposed commits instead of discarding them.
            // Each tx was dequeued from the pool as `Validated` by `propose_blocks`, so it is already
            // in the registry — the same commit routine as the inbound Commit path (4.1a) commits it.
            let commits = self.propose_blocks(&token_id, 10).await?;
            for commit in commits {
                if let Err(e) = self
                    .check_and_commit_transaction_results(&commit, self.sender_public_key().clone())
                    .await
                {
                    self.logger().log(format!("Leader-propose commit error: {:?}", e));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use dashmap::DashMap;

    use pneumatic_core::blocks::Block;
    use pneumatic_core::config::Config;
    use pneumatic_core::crypto::{AsymCryptoProvider, BasicHashProvider, Ed25519Provider};
    use pneumatic_core::data::{DataError, DataProvider, StubDataProvider};
    use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::epoch::{BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector, ExecutorSet};
    use pneumatic_core::errors::TransactionRiskFactor;
    use pneumatic_core::gossiper::Gossiper;
    use pneumatic_core::messages::Message;
    use pneumatic_core::node::registry::NodeRegistry;
    use pneumatic_core::node::NodeRegistryType;
    use pneumatic_core::registry::PendingTransactionRegistry;
    use pneumatic_core::transactions::{PendingTransaction, SignedTransaction, Transaction, TransactionCommit, TransactionSignature, TransactionState, TransactionValidationResult};
    use pneumatic_core::user::User;

    use crate::committer_error::CommitterError;
    use super::*;

    // --- In-memory DataProvider mock for tests ---

    struct TestDataProvider {
        users: Mutex<HashMap<Vec<u8>, HashMap<String, User>>>,
        /// When true, `get_user` returns an error (simulates a data-service failure).
        fail_get: bool,
        /// When true, `save_user` returns an error (simulates a data-service failure).
        fail_save: bool,
        /// When true, `save_stake_snapshot`/`save_executor_set` return an error
        /// (simulates a snapshot-persistence failure).
        fail_snapshot_save: bool,
    }

    impl TestDataProvider {
        fn new() -> Self {
            Self {
                users: Mutex::new(HashMap::new()),
                fail_get: false,
                fail_save: false,
                fail_snapshot_save: false,
            }
        }

        /// `new()` with both simulated data-service failures armed.
        fn with_failures(fail_get: bool, fail_save: bool) -> Self {
            Self {
                users: Mutex::new(HashMap::new()),
                fail_get,
                fail_save,
                fail_snapshot_save: false,
            }
        }

        /// Arm the stake/executor snapshot-persistence failure, so `save_stake_snapshot`
        /// and `save_executor_set` return `Err`. Used to prove `advance_epoch_to` surfaces a
        /// persistence error rather than swallowing it (AUDIT Phase 5.4 / M8).
        fn with_snapshot_save_failure(mut self, fail: bool) -> Self {
            self.fail_snapshot_save = fail;
            self
        }
        fn insert_user(&self, key: Vec<u8>, partition_id: String, user: User) {
            self.users
                .lock()
                .unwrap()
                .entry(key)
                .or_default()
                .insert(partition_id, user);
        }

        /// Read a user's stored balance directly from the backing map, bypassing the fail toggles.
        /// Lets an assertion confirm a value survived a simulated data-service failure (when the
        /// normal `get_user` path is deliberately returning `Err`).
        fn raw_balance(&self, key: &[u8], partition_id: &str) -> Option<u64> {
            self.users
                .lock()
                .unwrap()
                .get(key)
                .and_then(|partitions| partitions.get(partition_id))
                .map(|u| u.fuel_balance)
        }
    }

    impl DataProvider for TestDataProvider {
        fn get_token(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Token, DataError> {
            Err(DataError::DataNotFound)
        }
        fn save_token(&self, _key: &Vec<u8>, _token: Token, _partition_id: &str) -> Result<(), DataError> {
            Ok(())
        }
        fn save_data(&self, _key: &Vec<u8>, _data: Vec<u8>, _partition_id: &str) -> Result<(), DataError> {
            Ok(())
        }
        fn get_user(&self, key: &Vec<u8>, partition_id: &str) -> Result<User, DataError> {
            if self.fail_get {
                return Err(DataError::StoreNotFound);
            }
            self.users
                .lock()
                .unwrap()
                .get(key)
                .and_then(|partitions| partitions.get(partition_id))
                .cloned()
                .ok_or(DataError::DataNotFound)
        }
        fn save_user(&self, key: &Vec<u8>, user: User, partition_id: &str) -> Result<(), DataError> {
            if self.fail_save {
                return Err(DataError::StoreNotFound);
            }
            self.users
                .lock()
                .unwrap()
                .entry(key.clone())
                .or_default()
                .insert(partition_id.to_string(), user);
            Ok(())
        }

        fn get_stake_snapshot(&self, _epoch: u64, _partition_id: &str) -> Result<StakeSet, DataError> {
            Ok(StakeSet::default())
        }

        fn save_stake_snapshot(&self, _epoch: u64, _snapshot: StakeSet, _partition_id: &str) -> Result<(), DataError> {
            if self.fail_snapshot_save {
                return Err(DataError::StoreNotFound);
            }
            Ok(())
        }

        fn get_executor_set(&self, _epoch: u64, _partition_id: &str) -> Result<ExecutorSet, DataError> {
            Ok(ExecutorSet::default())
        }

        fn save_executor_set(&self, _epoch: u64, _set: ExecutorSet, _partition_id: &str) -> Result<(), DataError> {
            if self.fail_snapshot_save {
                return Err(DataError::StoreNotFound);
            }
            Ok(())
        }
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
        EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec")
    }

    /// The canonical test `Transaction` shared by every block/entry builder in this module. Built
    /// in one place so the committed block (`make_test_block_for_token` / `make_block_with_proposer`)
    /// and the pending registry entry (`make_finalizing_entry` / `make_validated_entry`) carry a
    /// byte-identical payload. This is now required: the Committer rejects a commit whose block
    /// embeds a transaction that differs from the validated one (AUDIT Phase 3.5 / H12), so a
    /// committed block and its registry entry must hash equal.
    fn make_test_transaction(tx_id: &str, sender: Vec<u8>) -> Transaction {
        Transaction {
            id: tx_id.to_string(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender,
            receiver: vec![2],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        }
    }

    /// Create a block that chains off the token's current chain state.
    fn make_test_block_for_token(
        committer: &Committer,
        trans_id: &str,
        sender: Vec<u8>,
    ) -> Block {
        make_block_for_token_id(committer, trans_id, sender, &vec![1])
    }

    /// Generic variant of `make_test_block_for_token` that reads the chain state of an
    /// arbitrary `token_id`, so tests can build a block for a second, independent token
    /// (each chaining off its own genesis).
    fn make_block_for_token_id(
        committer: &Committer,
        trans_id: &str,
        sender: Vec<u8>,
        token_id: &[u8],
    ) -> Block {
        // Get the chain's last hash (empty previous_hash at genesis)
        let prev_hash = if let Some(entry) = committer.tokens.get(token_id) {
            let token = entry.value();
            let state = token.blockchain.get_current_chain_state();
            if state.last_hash_in.is_empty() {
                // Genesis convention: block 1 has an empty previous_hash
                Vec::<u8>::new()
            } else {
                state.last_hash_in
            }
        } else {
            Vec::<u8>::new()
        };

        let signed = SignedTransaction {
            transaction_id: trans_id.to_string(),
            transaction: make_test_transaction(trans_id, sender),
            total_voters: 3,
            total_stake: 42,
            leader_hash: prev_hash.clone(),
            leader_address: vec![],
            leader_stake: 0,
            finalizer_addr: vec![],
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![],
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: vec![],
        };

        let mut block = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block);
        block
    }

    fn bootstrap_token_chain(committer: &Committer) {
        // Pre-seed a genesis block so these tests exercise the
        // non-empty-chain path. Also mark the token self-verified: with
        // fail-closed block validation (AUDIT Phase 3.2 / C5) a process-style
        // block committed on this chain only validates under the "SelfSigned"
        // spec, which gates on token.is_self_verified.
        if let Some(mut entry) = committer.tokens.get_mut(&vec![1]) {
            entry.value_mut().is_self_verified = true;
        }

        let prev_hash = vec![42u8; 32];
        let signed = SignedTransaction::test_transaction();
        let mut genesis = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        genesis.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&genesis);

        if let Some(mut entry) = committer.tokens.get_mut(&vec![1]) {
            entry.value_mut().blockchain.add_block(genesis);
        }
    }

    /// Builds a Committer with the default full-stake slash fraction (1.0).
    fn make_test_committer(
        data_provider: Arc<TestDataProvider>,
    ) -> (Committer, Arc<PendingTransactionRegistry>, Arc<CollectingLogger>) {
        make_test_committer_with_slash(data_provider, 1.0)
    }

    /// Builds a Committer with an overridable `CostModel.slash_fraction`. The default
    /// `make_test_committer` delegates here with the full-stake default (1.0).
    fn make_test_committer_with_slash(
        data_provider: Arc<TestDataProvider>,
        slash_fraction: f64,
    ) -> (Committer, Arc<PendingTransactionRegistry>, Arc<CollectingLogger>) {
        let mut env_data = Arc::new(make_test_env_data());
        // Install an in-memory collecting logger so a test can assert a failure path emitted an
        // observable log line (the default FileLogger discards to a file). `env_data` is uniquely
        // owned right here, so `Arc::get_mut` reaches the metadata to swap the logger without
        // changing EnvironmentMetadata's shape or the public API. The slash fraction is set here too
        // so a test can drive a partial (vs. full) slash of a double-signed proposer's stake.
        let logger = CollectingLogger::default();
        {
            let env = Arc::get_mut(&mut env_data)
                .expect("env_data uniquely owned at construction time");
            env.logger = Arc::new(logger.clone());
            env.cost_model.slash_fraction = slash_fraction;
        }
        let identity = Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory());
        let rhash = identity.rhash;
        let config = Config {
            public_key: vec![1],
            ip_address: "127.0.0.1".parse().unwrap(),
            rest_api_version: 1,
            node_type: pneumatic_core::node::NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            // Capacity entries are required — without them get_max_node_number
            // returns 0 and register_peer rejects every peer.
            type_configs: Arc::new({
                let tc = DashMap::new();
                tc.insert(NodeRegistryType::Committer.clone(),
                    pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 });
                tc.insert(NodeRegistryType::Sentinel.clone(),
                    pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 });
                tc.insert(NodeRegistryType::Archiver.clone(),
                    pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 });
                tc
            }),
            identity: identity.clone(),
            rhash,
            bootstrap_peers: Vec::new(),
            rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
            transport_enabled: false,
        };
        let node_registry = Arc::new(NodeRegistry::init(
            Arc::new(config),
            None,
            Arc::new(|_, _| true),
        ));

        let gossiper_identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
        let gossiper_rhash = gossiper_identity.rhash;
        let gossiper = Arc::new(Gossiper::new(
            NodeRegistryType::Committer,
            Config {
                public_key: vec![2],
                ip_address: "127.0.0.1".parse().unwrap(),
                rest_api_version: 1,
                node_type: pneumatic_core::node::NodeType::Full,
                node_registry_types: vec![NodeRegistryType::Committer],
                main_environment_id: "test".to_string(),
                reconciliation_partition_id: "recon".to_string(),
                environment_metadata: Arc::new(DashMap::new()),
                type_configs: Arc::new(DashMap::new()),
                identity: Arc::new(gossiper_identity),
                rhash: gossiper_rhash,
                bootstrap_peers: Vec::new(),
                rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
                transport_enabled: false,
            },
            60,
            env_data.asym_crypto_provider.clone(),
        ));

        let tokens = Arc::new(DashMap::new());
        let pending_registry = Arc::new(PendingTransactionRegistry::new());
        let stake_store = Arc::new(StakeStore::new());
        let staking_manager = Arc::new(StakingManager::new(stake_store.clone(), env_data.logger.clone()));
        let data_provider_core = Arc::new(pneumatic_core::data::DefaultDataProvider::new());
        let candidate_registry = Arc::new(CandidateRegistry::new());
        let epoch_reconciler = Arc::new(EpochReconciler::new(
            stake_store.clone(),
            candidate_registry.clone(),
            data_provider_core.clone(),
            "test".to_string(),
            vec![vec![1]], // token ID from bootstrap_token
            env_data.cost_model.slash_fraction,
        ));
        let hash_provider = Arc::new(BasicHashProvider::new());
        let leader_selector = Arc::new(LeaderSelector::new(hash_provider));

        // Epoch tracking components
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let epoch_duration = 300;
        let initial_epoch = Epoch::new_with_leader(
            1,
            now,
            now + epoch_duration,
            leader_selector.as_ref(),
            &stake_store.to_stake_set(),
            &[], // genesis: no prior block → empty prev_block_hash
        );
        let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
        let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));

        let block_services = Arc::new(BlockServices::new(
            tokens.clone(),
            data_provider_core.clone(),
            node_registry.clone(),
            env_data.clone(),
            env_data.logger.clone(),
            identity.clone(),
        ));

        let committer = Committer::new(
            env_data.clone(),
            vec![1],
            identity,
            gossiper,
            block_services,
            node_registry,
            tokens,
            pending_registry.clone(),
            stake_store,
            staking_manager,
            epoch_reconciler,
            leader_selector,
            data_provider,
            0,
            Some(epoch_detector),
            block_proposer,
            epoch_duration,
            5000,
            candidate_registry,
        );

        (committer, pending_registry, Arc::new(logger))
    }

    fn make_finalizing_entry(
        pending_registry: &PendingTransactionRegistry,
        tx_id: &str,
        sender: Vec<u8>,
    ) {
        pending_registry.register_pending(tx_id.to_string()).unwrap();
        let tx = make_test_transaction(tx_id, sender.clone());
        // Transition directly via the internal map — register_pending creates Pending,
        // then we mutate to Finalizing state.
        {
            let mut entry = pending_registry.get_transaction_mut(tx_id).unwrap();
            entry.transition_to_validated(tx.clone(),
                pneumatic_core::transactions::TransactionValidationResult {
                    is_valid: true,
                    risk: pneumatic_core::errors::TransactionRiskFactor {
                        affected_parties: 2, amount: 100,
                        is_contract: false, is_multi_party: false,
                    },
                    failure_reasons: vec![],
                    finalizer_public_key: vec![3],
                });
            entry.transition_to_finalizing(tx, vec![3]);
        }
    }

    // --- Gas deduction tests ---

    #[tokio::test]
    async fn check_and_commit_deducts_gas_from_user() {
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
            public_key: b"alice".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        // Bootstrap token and chain BEFORE creating block
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_gas_deduct";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
        registry.record_gas_used(tx_id, 50);

        let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
        if let Err(ref e) = result {
            eprintln!("Error: {:?}", e);
        }
        assert!(result.is_ok());

        let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 950);
    }

    #[tokio::test]
    async fn check_and_commit_no_gas_tracked_does_not_deduct() {
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"bob".to_vec(), "token".to_string(), User {
            public_key: b"bob".to_vec(),
            fuel_balance: 500,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        // Bootstrap token and chain BEFORE creating block
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_no_gas";
        make_finalizing_entry(&registry, tx_id, b"bob".to_vec());
        // No record_gas_used called

        let block = make_test_block_for_token(&committer, tx_id, b"bob".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
        assert!(result.is_ok());

        let user = dp.get_user(&b"bob".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 500);
    }

    #[tokio::test]
    async fn check_and_commit_gas_exceeds_balance_saturates() {
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"charlie".to_vec(), "token".to_string(), User {
            public_key: b"charlie".to_vec(),
            fuel_balance: 100,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        // Bootstrap token and chain BEFORE creating block
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_sat";
        make_finalizing_entry(&registry, tx_id, b"charlie".to_vec());
        registry.record_gas_used(tx_id, 200);

        let block = make_test_block_for_token(&committer, tx_id, b"charlie".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
        assert!(result.is_ok());

        let user = dp.get_user(&b"charlie".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 0);
    }

    // --- AUDIT Phase 4.5 / M11: a failed gas deduction is surfaced and observable, and the
    //     per-sender read-modify-write is serialized so concurrent commits cannot lose an update ---

    /// In-memory Logger for tests: captures every `log` call so a test can assert that a
    /// failure path produced an observable line (the real FileLogger discards to a file).
    #[derive(Default, Clone)]
    struct CollectingLogger {
        logs: Arc<Mutex<Vec<String>>>,
    }

    impl Logger for CollectingLogger {
        fn log(&self, message: String) {
            self.logs.lock().unwrap().push(message);
        }
    }

    /// Assert `result` is a `GasDeduction` error carrying the expected sender / tx_id / gas.
    fn assert_gas_deduction(
        result: Result<(), CommitterError>,
        sender: &[u8],
        tx_id: &str,
        gas_used: u64,
    ) {
        match result {
            Err(CommitterError::GasDeduction {
                sender: got_sender,
                tx_id: got_tx_id,
                gas_used: got_gas,
                ..
            }) => {
                assert_eq!(got_sender, bytes_to_hex(sender));
                assert_eq!(got_tx_id, tx_id);
                assert_eq!(got_gas, gas_used);
            }
            other => panic!("expected CommitterError::GasDeduction, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn save_user_failure_is_reported_not_swallowed() {
        let dp = Arc::new(TestDataProvider::with_failures(false, true));
        dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
            public_key: b"alice".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_save_fail";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
        registry.record_gas_used(tx_id, 50);

        let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        // M11: a failed `save_user` must surface as `GasDeduction` — the old `let _ =` swallowed
        // it, letting the tx reach Committed with no debit. The committed block stands (it was
        // validated/finalized); the failure is returned, not dropped.
        let result = committer.check_and_commit_transaction_results(&commit, vec![3]).await;
        assert_gas_deduction(result, b"alice", tx_id, 50);

        // The sender was never debited, so the tx must stay Finalizing (observable as a failed
        // settlement), NOT Committed.
        let entry = registry.get_transaction_mut(tx_id).unwrap();
        assert!(
            matches!(entry.state, TransactionState::Finalizing { .. }),
            "tx must stay Finalizing after a failed deduction, got {:?}",
            entry.state
        );

        // No gas given for free — balance unchanged from the pre-commit value.
        let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 1000);
    }

    #[tokio::test]
    async fn get_user_failure_is_reported_not_swallowed() {
        let dp = Arc::new(TestDataProvider::with_failures(true, false));
        dp.insert_user(b"bob".to_vec(), "token".to_string(), User {
            public_key: b"bob".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_get_fail";
        make_finalizing_entry(&registry, tx_id, b"bob".to_vec());
        registry.record_gas_used(tx_id, 50);

        let block = make_test_block_for_token(&committer, tx_id, b"bob".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        // M11: a failed `get_user` (blocked data service) must surface too, not be silently
        // skipped (the old `if let Ok(mut user)` skipped the whole deduction).
        let result = committer.check_and_commit_transaction_results(&commit, vec![3]).await;
        assert_gas_deduction(result, b"bob", tx_id, 50);

        let entry = registry.get_transaction_mut(tx_id).unwrap();
        assert!(
            matches!(entry.state, TransactionState::Finalizing { .. }),
            "tx must stay Finalizing after a failed deduction, got {:?}",
            entry.state
        );

        // get_user is deliberately failing here, so read the stored value directly to confirm the
        // balance was never touched — gas was neither deducted nor, as the fail-closed rule requires,
        // silently freed (the tx is not Committed either).
        assert_eq!(dp.raw_balance(&b"bob".to_vec(), "token"), Some(1000));
    }

    #[tokio::test]
    async fn gas_deduction_failure_logs() {
        let dp = Arc::new(TestDataProvider::with_failures(false, true));
        dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
            public_key: b"alice".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, collector) = make_test_committer(dp.clone());

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_log";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
        registry.record_gas_used(tx_id, 50);

        let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, vec![3]).await;
        assert!(matches!(result, Err(CommitterError::GasDeduction { .. })));

        let logs = collector.logs.lock().unwrap();
        let hit = logs
            .iter()
            .find(|l| l.contains("GAS DEDUCTION FAILED"))
            .expect("expected an observable 'GAS DEDUCTION FAILED' log line");
        // The failure line carries the sender (hex), tx id, gas used, and the error cause.
        assert!(hit.contains(&bytes_to_hex(b"alice")), "log was: {hit}");
        assert!(hit.contains(tx_id), "log was: {hit}");
        assert!(hit.contains("50"), "log was: {hit}");
        assert!(hit.contains("StoreNotFound"), "log was: {hit}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_same_sender_commits_both_deduct() {
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
            public_key: b"alice".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        // Two independent tokens, each with its own genesis-seeded, self-verified chain, let both
        // commits run concurrently without colliding on block linkage (they share only the sender).
        // Marking self_verified is required: fail-closed block validation (AUDIT Phase 3.2 / C5)
        // only accepts a process-style block on a chain whose token is self_verified.
        for id in [vec![1], vec![2]] {
            let mut token = Token::new();
            token.id = id.clone();
            token.is_self_verified = true;
            committer.bootstrap_token(token);

            let mut genesis = Block {
                signed_trans: SignedTransaction::test_transaction(),
                token_metadata: HashMap::new(),
                previous_hash: vec![42u8; 32],
                current_hash: vec![],
                timestamp: 0,
                finality_status: FinalityStatus::Optimistic,
                proposer_key: vec![],
                epoch_number: 0,
            };
            let genesis_hash = pneumatic_core::blocks::BlockFactory::create_hash(&genesis);
            if let Some(mut entry) = committer.tokens.get_mut(&id) {
                genesis.current_hash = genesis_hash;
                entry.value_mut().blockchain.add_block(genesis);
            }
        }

        for tx_id in ["tx_conc_1", "tx_conc_2"] {
            make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
            registry.record_gas_used(tx_id, 50);
        }

        let block1 = make_block_for_token_id(&committer, "tx_conc_1", b"alice".to_vec(), &vec![1]);
        let commit1 = TransactionCommit {
            trans_id: b"tx_conc_1".to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block1,
        };
        let block2 = make_block_for_token_id(&committer, "tx_conc_2", b"alice".to_vec(), &vec![2]);
        let commit2 = TransactionCommit {
            trans_id: b"tx_conc_2".to_vec(),
            token_id: vec![2],
            env_id: "test".to_string(),
            proposed_block: block2,
        };

        // Fire both commits for the SAME sender concurrently on a two-worker runtime, so the
        // get_user -> subtract -> save_user read-modify-write can genuinely race.
        let f1 = committer.check_and_commit_transaction_results(&commit1, vec![3]);
        let f2 = committer.check_and_commit_transaction_results(&commit2, vec![3]);
        let (r1, r2) = tokio::join!(f1, f2);
        assert!(r1.is_ok(), "commit1 failed: {r1:?}");
        assert!(r2.is_ok(), "commit2 failed: {r2:?}");

        // With the per-sender RMW lock the two 50-unit deductions serialize: 1000 -> 900.
        // Without the lock, last-write-wins loses one deduction (balance would be 950), which
        // is the exact lost-update M11 guards against.
        let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 900);
    }

    fn make_validated_entry(
        pending_registry: &PendingTransactionRegistry,
        tx_id: &str,
        sender: Vec<u8>,
    ) {
        pending_registry.register_pending(tx_id.to_string()).unwrap();
        let tx = make_test_transaction(tx_id, sender.clone());
        // Transition to Validated (NOT Finalizing) — simulates leader-proposal path
        {
            let mut entry = pending_registry.get_transaction_mut(tx_id).unwrap();
            entry.transition_to_validated(tx.clone(),
                pneumatic_core::transactions::TransactionValidationResult {
                    is_valid: true,
                    risk: pneumatic_core::errors::TransactionRiskFactor {
                        affected_parties: 2, amount: 100,
                        is_contract: false, is_multi_party: false,
                    },
                    failure_reasons: vec![],
                    finalizer_public_key: vec![3],
                });
        }
    }

    #[tokio::test]
    async fn check_and_commit_validated_state_succeeds() {
        // Leader-proposal path: transaction is in Validated state (not Finalizing).
        // The Committer should still commit the block and transition to Committed.
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
            public_key: b"alice".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_leader_proposal";
        make_validated_entry(&registry, tx_id, b"alice".to_vec());
        registry.record_gas_used(tx_id, 75);

        let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
        assert!(result.is_ok());

        // Gas was deducted
        let user = dp.get_user(&b"alice".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 925);

        // Transaction was removed from pool (leader-proposal path)
        assert!(!registry.contains(tx_id));
    }

    #[tokio::test]
    async fn check_and_commit_validated_saturates_on_overflow() {
        // Leader-proposal path with gas exceeding balance — should saturate to 0
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"bob".to_vec(), "token".to_string(), User {
            public_key: b"bob".to_vec(),
            fuel_balance: 50,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_leader_overflow";
        make_validated_entry(&registry, tx_id, b"bob".to_vec());
        registry.record_gas_used(tx_id, 200); // exceeds balance

        let block = make_test_block_for_token(&committer, tx_id, b"bob".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
        assert!(result.is_ok());

        let user = dp.get_user(&b"bob".to_vec(), "token").unwrap();
        assert_eq!(user.fuel_balance, 0);
    }

    // --- propose_blocks and advance_epoch tests ---

    /// Build a Committer where the leader is controlled by `leader_key`.
    /// The committer's own public key is `committer_key`.
    fn make_committer_for_leader_test(
        committer_key: Vec<u8>,
        leader_key: Vec<u8>,
    ) -> (Committer, Arc<PendingTransactionRegistry>, Arc<TestDataProvider>) {
        build_committer_for_leader_test(committer_key, leader_key, Arc::new(TestDataProvider::new()))
    }

    /// The full committer construction for leader/epoch tests, parameterized on the injected
    /// data provider. The public `make_committer_for_leader_test` wraps this with a default
    /// provider so existing call sites are unaffected; tests that need a failing provider
    /// (e.g. snapshot-persistence failures, AUDIT Phase 5.4 / M8) call this directly.
    fn build_committer_for_leader_test(
        committer_key: Vec<u8>,
        leader_key: Vec<u8>,
        dp: Arc<TestDataProvider>,
    ) -> (Committer, Arc<PendingTransactionRegistry>, Arc<TestDataProvider>) {
        let env_data = Arc::new(make_test_env_data());
        let identity = Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory());
        let rhash = identity.rhash;
        let config = Config {
            public_key: committer_key.clone(),
            ip_address: "127.0.0.1".parse().unwrap(),
            rest_api_version: 1,
            node_type: pneumatic_core::node::NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            type_configs: Arc::new(DashMap::new()),
            identity: identity.clone(),
            rhash,
            bootstrap_peers: Vec::new(),
            rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
            transport_enabled: false,
        };
        let node_registry = Arc::new(NodeRegistry::init(
            Arc::new(config),
            None,
            Arc::new(|_, _| true),
        ));

        let gossiper_identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
        let gossiper_rhash = gossiper_identity.rhash;
        let gossiper = Arc::new(Gossiper::new(
            NodeRegistryType::Committer,
            Config {
                public_key: vec![2],
                ip_address: "127.0.0.1".parse().unwrap(),
                rest_api_version: 1,
                node_type: pneumatic_core::node::NodeType::Full,
                node_registry_types: vec![NodeRegistryType::Committer],
                main_environment_id: "test".to_string(),
                reconciliation_partition_id: "recon".to_string(),
                environment_metadata: Arc::new(DashMap::new()),
                type_configs: Arc::new(DashMap::new()),
                identity: Arc::new(gossiper_identity),
                rhash: gossiper_rhash,
                bootstrap_peers: Vec::new(),
                rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
                transport_enabled: false,
            },
            60,
            env_data.asym_crypto_provider.clone(),
        ));

        let tokens = Arc::new(DashMap::new());
        let pending_registry = Arc::new(PendingTransactionRegistry::new());
        let stake_store = Arc::new(StakeStore::new());
        let staking_manager = Arc::new(StakingManager::new(stake_store.clone(), env_data.logger.clone()));
        let data_provider_core = Arc::new(pneumatic_core::data::DefaultDataProvider::new());
        let candidate_registry = Arc::new(CandidateRegistry::new());
        let epoch_reconciler = Arc::new(EpochReconciler::new(
            stake_store.clone(),
            candidate_registry.clone(),
            data_provider_core.clone(),
            "test".to_string(),
            vec![vec![1]],
            env_data.cost_model.slash_fraction,
        ));
        let hash_provider = Arc::new(BasicHashProvider::new());
        let leader_selector = Arc::new(LeaderSelector::new(hash_provider));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let epoch_duration = 3600; // 1 hour — is_epoch_expired returns false
        let initial_epoch = Epoch {
            epoch_number: 1,
            start_timestamp: now,
            end_timestamp: now + epoch_duration,
            leader_public_key: leader_key.clone(),
        };
        let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
        let block_proposer = Arc::new(BlockProposer::new(leader_key, 100, vec![]));

        let block_services = Arc::new(BlockServices::new(
            tokens.clone(),
            data_provider_core.clone(),
            node_registry.clone(),
            env_data.clone(),
            env_data.logger.clone(),
            identity.clone(),
        ));

        let test_dp = dp;
        let committer = Committer::new(
            env_data,
            committer_key,
            identity,
            gossiper,
            block_services,
            node_registry,
            tokens,
            pending_registry.clone(),
            stake_store,
            staking_manager,
            epoch_reconciler,
            leader_selector,
            test_dp.clone(),
            1,
            Some(epoch_detector),
            block_proposer,
            epoch_duration,
            5000,
            candidate_registry,
        );

        (committer, pending_registry, test_dp)
    }

    #[tokio::test]
    async fn propose_blocks_returns_empty_when_not_leader() {
        let (committer, _registry, _dp) = make_committer_for_leader_test(
            vec![99], // committer key
            b"leader".to_vec(), // leader key — different
        );

        let result = committer.propose_blocks(&[1], 10).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn propose_blocks_returns_batch_when_leader_with_pool_items() {
        let (committer, registry, _dp) = make_committer_for_leader_test(
            b"leader".to_vec(), // committer key
            b"leader".to_vec(), // leader key — same, so this IS the leader
        );

        // Bootstrap token so it's in the cache
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);

        // Add a transaction to the pool
        let tx_id = "tx_propose_1".to_string();
        registry.register_pending(tx_id.clone()).unwrap();
        let tx = Transaction {
            id: tx_id.clone(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b"sender".to_vec(),
            receiver: b"receiver".to_vec(),
            amount: Some(50),
            timestamp: 5000,
            result_hash: vec![],
            sender_signature: vec![],
        };
        registry.transition_to_validated_and_enqueue(
            &tx_id,
            tx.clone(),
            TransactionValidationResult {
                is_valid: true,
                risk: TransactionRiskFactor {
                    affected_parties: 2,
                    amount: 50,
                    is_contract: false,
                    is_multi_party: false,
                },
                failure_reasons: vec![],
                finalizer_public_key: vec![5],
            },
        ).unwrap();

        // This committer IS the leader and has pool items — should return a batch
        let result = committer.propose_blocks(&[1], 10).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].trans_id, tx_id.into_bytes());
    }

    #[tokio::test]
    async fn run_epoch_loop_commits_leader_proposed_block() {
        // AUDIT Phase 4.1 / H4 (4.1c): run_epoch_loop must consume propose_blocks output and
        // commit each leader-proposed block — not discard it. A committer that is the leader and
        // has a Validated tx in the pool must grow the chain. Pre-fix (`let _ = self.propose_blocks(...)`)
        // discarded every proposed commit, so the chain never grew; this asserts it does.
        let (committer, registry, _dp) = make_committer_for_leader_test(
            b"leader".to_vec(), // committer key
            b"leader".to_vec(), // leader key — same, so this IS the leader
        );

        // Bootstrap token + genesis chain so commit_block has a validated chain to append to.
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        // Add a Validated tx to the pool — this is what propose_blocks dequeues for the leader.
        let tx_id = "tx_epoch_commit".to_string();
        registry.register_pending(tx_id.clone()).unwrap();
        registry
            .transition_to_validated_and_enqueue(
                &tx_id,
                Transaction {
                    id: tx_id.clone(),
                    action: "Process".into(),
                    token_id: vec![1],
                    bid: None,
                    sequence_number: 1,
                    sender: b"alice".to_vec(),
                    receiver: b"bob".to_vec(),
                    amount: Some(100),
                    timestamp: 0,
                    result_hash: vec![],
                    sender_signature: vec![],
                },
                TransactionValidationResult {
                    is_valid: true,
                    risk: TransactionRiskFactor {
                        affected_parties: 2,
                        amount: 100,
                        is_contract: false,
                        is_multi_party: false,
                    },
                    failure_reasons: vec![],
                    finalizer_public_key: vec![5],
                },
            )
            .unwrap();

        let before = committer
            .tokens
            .get(&vec![1])
            .unwrap()
            .value()
            .blockchain
            .get_count();

        let result = committer.run_epoch_loop().await;
        assert!(result.is_ok(), "run_epoch_loop failed: {:?}", result.err());

        // The leader-proposed commit must have been consumed and committed — the chain grew.
        let after = committer
            .tokens
            .get(&vec![1])
            .unwrap()
            .value()
            .blockchain
            .get_count();
        assert!(
            after >= before + 1,
            "run_epoch_loop should have committed a leader block ({} -> {})",
            before,
            after
        );
    }


    #[tokio::test]
    async fn advance_epoch_bumps_number() {
        let (committer, _registry, _dp) = make_committer_for_leader_test(
            b"leader".to_vec(),
            b"leader".to_vec(),
        );

        // Add a staker so leader selection produces a non-empty result
        committer.stake_store.add_staker(b"leader".to_vec(), 100);

        let before = committer.current_epoch_number.load(Ordering::SeqCst);
        let new_leader = committer.advance_epoch().unwrap();

        let after = committer.current_epoch_number.load(Ordering::SeqCst);
        assert_eq!(after, before + 1);
        assert!(!new_leader.unwrap().is_empty());
    }

    #[tokio::test]
    async fn advance_epoch_leader_changes_with_mined_tip() {
        // AUDIT Phase 5.3 / H3 discriminator: the leader a committer selects when
        // advancing to a new epoch must depend on the mined chain tip it holds
        // locally in its token cache, so the next epoch's leader is only
        // knowable once this tip is produced. This committer holds TWO real
        // mined tips (two internally-consistent blocks with different contents)
        // and selects a different leader for each; the bug (leader seed bound to
        // the empty/stale persisted tip instead of the mined one) ignores the
        // local tip, so both would select the same leader and this test fails.
        //
        // A many-staker spread is used so the two distinct mined tips — which are
        // fixed hashes we cannot control — land on different stakers deterministically
        // rather than by luck of a 50/50 draw.
        fn committer_with_blocks(n_blocks: u64) -> Committer {
            let (committer, _registry, _dp) = make_committer_for_leader_test(
                b"leader".to_vec(),
                b"leader".to_vec(),
            );
            // 256 spread stakers (unique keys, 1 stake each) — the tip's derived
            // target is uniform over a large space, so two distinct tips almost
            // certainly select different leaders.
            for i in 0..256 {
                committer.stake_store.add_staker(vec![i as u8], 1);
            }
            let mut token = Token::new();
            token.id = vec![1];
            token.is_self_verified = true;
            token.environment_id = "test".to_string();
            let mut previous_hash: Vec<u8> = vec![];
            for _ in 0..n_blocks {
                let mut block = Block {
                    signed_trans: SignedTransaction::test_transaction(),
                    token_metadata: HashMap::new(),
                    previous_hash,
                    timestamp: 0,
                    current_hash: vec![],
                    finality_status: FinalityStatus::Optimistic,
                    proposer_key: vec![],
                    epoch_number: 0,
                };
                block.current_hash = BlockFactory::create_hash(&block);
                previous_hash = block.current_hash.clone();
                token.blockchain.add_block(block);
            }
            committer.bootstrap_token(token);
            committer
        }

        // Same epoch (both advance 1 -> 2), same stake, same starting leader, but
        // different mined tips: one chain holds no block, one holds two blocks.
        let leader_empty = committer_with_blocks(0).advance_epoch().unwrap();
        let leader_two_blocks = committer_with_blocks(2).advance_epoch().unwrap();

        assert_ne!(
            leader_empty,
            leader_two_blocks,
            "leader must depend on the mined chain tip"
        );
    }

    #[tokio::test]
    async fn advance_epoch_to_never_rewinds_or_reuses() {
        // AUDIT Phase 5.4 / H9 discriminator: `advance_epoch_to` is the single writer of the
        // epoch number. Two consecutive advances are strictly increasing (never reuse a number)
        // and the mirrored counter tracks the authoritative detector; and a target that does not
        // strictly exceed the current value is refused — a replayed/rewinding advance is a no-op,
        // never a rewind. Reverting the guard (`if stored >= new { return Ok(None) }`) lets a
        // seeded-ahead counter get overwritten, so this test fails on the buggy code.
        let (committer, _registry, _dp) =
            make_committer_for_leader_test(b"leader".to_vec(), b"leader".to_vec());
        // Spread stakers so each epoch's derived seed lands on a distinct leader.
        for i in 0..256 {
            committer.stake_store.add_staker(vec![i as u8], 1);
        }

        // Two sequential advances: 1 -> 2 -> 3, strictly increasing, both non-empty leaders.
        let leader_1 = committer.advance_epoch().unwrap().expect("advanced to 2");
        let epoch_1 = committer.current_epoch_number.load(Ordering::SeqCst);
        let leader_2 = committer.advance_epoch().unwrap().expect("advanced to 3");
        let epoch_2 = committer.current_epoch_number.load(Ordering::SeqCst);
        assert!(!leader_1.is_empty());
        assert!(!leader_2.is_empty());
        assert_eq!(epoch_1, 2);
        assert_eq!(epoch_2, 3);
        assert_ne!(epoch_1, epoch_2, "advances must never reuse an epoch number");

        // The detector is authoritative: the mirrored counter matches its epoch. Scoped in a
        // block so the guard is dropped before the next advance acquires the detector lock.
        {
            let guard = committer.epoch_detector.lock().await;
            assert_eq!(
                guard.as_ref().unwrap().current_epoch.epoch_number,
                epoch_2,
                "detector epoch must track the mirrored counter"
            );
        }

        // Seed the counter ahead of the detector — exactly the divergence the single writer
        // removes. A replayed advance to a stale target (epoch 4) must be refused, never
        // overwrite the counter back down to 4.
        committer.current_epoch_number.store(1000, Ordering::SeqCst);
        assert!(
            committer.advance_epoch().unwrap().is_none(),
            "advance to epoch 4 must be refused when the stored counter (1000) is ahead"
        );
        let guard = committer.epoch_detector.lock().await;
        assert_eq!(
            guard.as_ref().unwrap().current_epoch.epoch_number, 3,
            "a refused advance must not advance the detector"
        );
    }

    #[tokio::test]
    async fn advance_epoch_to_surfaces_snapshot_save_error() {
        // AUDIT Phase 5.4 / M8 discriminator: a stake/executor snapshot-persistence failure must
        // surface as `SnapshotPersist`, not be swallowed into `Ok(None)`. This provider fails
        // `save_stake_snapshot`/`save_executor_set`; advancing to a new epoch must return that
        // error. Reverting the `.map_err(...)` on the saves (the old `let _ =`) makes the advance
        // return Ok(None), so the test fails on the buggy code.
        let dp = Arc::new(TestDataProvider::new().with_snapshot_save_failure(true));
        let (committer, _registry, _dp) =
            build_committer_for_leader_test(b"leader".to_vec(), b"leader".to_vec(), dp);
        committer.stake_store.add_staker(b"leader".to_vec(), 100);

        let result = committer.advance_epoch();
        assert!(
            matches!(
                result,
                Err(CommitterError::SnapshotPersist { kind: "stake", .. })
            ),
            "advance_epoch must surface the snapshot-persistence failure, got {:?}",
            result
        );
    }

    // --- Conflict resolution at commit time ---

    /// Build a block with a specific proposer_key for conflict testing.
    fn make_block_with_proposer(
        committer: &Committer,
        trans_id: &str,
        proposer_key: Vec<u8>,
    ) -> Block {
        // Genesis convention: block 1 has an empty previous_hash
        let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
            let token = entry.value();
            let state = token.blockchain.get_current_chain_state();
            if state.last_hash_in.is_empty() {
                Vec::<u8>::new()
            } else {
                state.last_hash_in
            }
        } else {
            Vec::<u8>::new()
        };

        // The transaction sender is fixed to `alice` here (all commit-test callers register their
        // entry as alice); the only per-call variance is the `proposer_key`, which lives outside the
        // transaction payload. Build via the shared helper so the committed block hashes equal the
        // registry entry (AUDIT Phase 3.5 / H12).
        let signed = SignedTransaction {
            transaction_id: trans_id.to_string(),
            transaction: make_test_transaction(trans_id, b"alice".to_vec()),
            total_voters: 3,
            total_stake: 42,
            leader_hash: prev_hash.clone(),
            leader_address: vec![],
            leader_stake: 0,
            finalizer_addr: vec![],
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![],
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: proposer_key.clone(),
        };

        let mut block = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key,
            epoch_number: 0,
        };
        block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block);
        block
    }

    #[tokio::test]
    async fn commit_no_conflict_inserts_candidate() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_no_conflict";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

        let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        // Insert the existing candidate into the registry (simulating pre-existing)
        let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
            let token = entry.value();
            token.blockchain.get_current_chain_state().last_hash_in
        } else {
            vec![42u8; 32]
        };
        // Distinct stakes make the outcome deterministic: the incoming block
        // (proposer vec![], stake 500) wins over the existing candidate (proposer vec![10], stake 100).
        committer.stake_store.add_staker(vec![10], 100);
        committer.stake_store.add_staker(vec![], 500);

        let existing_block = make_block_with_proposer(&committer, "tx_existing", vec![10]);
        committer.candidate_registry.insert(
            vec![1], prev_hash.clone(), existing_block, vec![10],
        );

        // Commit — the conflict is detected and resolved. The winner commits and the
        // loser group is cleared (AUDIT Phase 5.2 / H2). The verified proposer (finalizer_key)
        // is the incoming block's self-declared proposer, vec![], matching its 500-stake identity.
        let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
        assert!(result.is_ok(), "higher-stake incoming block wins the conflict");
        assert_eq!(
            committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
            0,
            "the resolved loser candidate group must be cleared"
        );
        assert_eq!(
            committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
            2,
            "only the winning block commits; the loser is never appended"
        );
    }

    #[tokio::test]
    async fn commit_first_block_on_fresh_token_without_bootstrap() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp);

        // Bootstrap the token only — the chain stays empty
        let mut token = Token::new();
        token.id = vec![1];
        // Fail-closed validation (AUDIT Phase 3.2 / C5): the genesis commit only
        // validates under the "SelfSigned" spec when the token is flagged
        // is_self_verified.
        token.is_self_verified = true;
        committer.bootstrap_token(token);

        let tx_id = "tx_first_block";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

        // On an empty chain the helper emits previous_hash = vec![]
        // (genesis convention)
        let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
        assert!(block.previous_hash.is_empty());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        // Block 1 must commit through the standard path
        let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
        assert!(result.is_ok());

        let entry = committer.tokens.get(&vec![1]).unwrap();
        assert_eq!(entry.value().blockchain.get_count(), 1);
    }

    #[tokio::test]
    async fn commit_conflict_different_stakes_discards_loser() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_conflict_stake";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

        let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
            let token = entry.value();
            token.blockchain.get_current_chain_state().last_hash_in
        } else {
            vec![42u8; 32]
        };

        // Add proposers with different stakes to StakeStore
        committer.stake_store.add_staker(vec![10], 100);  // existing proposer (low stake)
        committer.stake_store.add_staker(b"alice".to_vec(), 500);  // new proposer (high stake)

        // Insert existing candidate with lower stake
        let existing_block = make_block_with_proposer(&committer, "tx_existing", vec![10]);
        committer.candidate_registry.insert(
            vec![1], prev_hash.clone(), existing_block, vec![10],
        );

        let block = make_block_with_proposer(&committer, tx_id, b"alice".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, b"alice".to_vec()).await;
        assert!(result.is_ok(), "higher-stake incoming block wins the conflict");

        // AUDIT Phase 5.2 / H2: the loser is discarded — only the winning block commits
        // (chain grew by exactly one) and the resolved candidate group is cleared, so
        // exactly one block remains at this position (no fork survives).
        assert_eq!(
            committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
            2,
            "only the winning block commits; the losing proposal is never appended"
        );
        assert_eq!(
            committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
            0,
            "the resolved loser candidate group must be cleared"
        );
    }

    #[tokio::test]
    async fn commit_conflict_same_proposer_emits_slash() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_double_sign";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

        let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
            let token = entry.value();
            token.blockchain.get_current_chain_state().last_hash_in
        } else {
            vec![42u8; 32]
        };

        // Same proposer — double-signed scenario
        committer.stake_store.add_staker(vec![10], 100);

        // Insert existing candidate with same proposer key
        let existing_block = make_block_with_proposer(&committer, "tx_existing", vec![10]);
        committer.candidate_registry.insert(
            vec![1], prev_hash.clone(), existing_block, vec![10],
        );

        let block = make_block_with_proposer(&committer, tx_id, vec![10]); // SAME proposer!
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, vec![10]).await;
        // AUDIT Phase 5.2 / H2: a same-proposer double-signed re-proposal is rejected
        // on the commit path (the winner stays as the tip, the loser is discarded).
        assert!(
            matches!(result, Err(CommitterError::LoserDiscarded)),
            "a double-signed re-proposal must be discarded on the commit path"
        );

        // AUDIT Phase 5.1 / H1: the slash is still applied even though the block is
        // rejected — the double-signed proposer's stake must actually drop to zero.
        assert_eq!(
            committer.stake_store.get_stake(&vec![10]),
            0,
            "full-stake slash should zero the offender's stake"
        );
        // And the resolved candidate group is cleared.
        assert_eq!(
            committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
            0,
            "the double-signed candidate group must be cleared"
        );
    }

    #[tokio::test]
    async fn commit_conflict_same_proposer_partial_slash_respects_fraction() {
        // AUDIT Phase 5.1 / H1: the slash amount is the configured fraction of
        // the offender's stake, not a hardcoded value. With slash_fraction = 0.5
        // a proposer staked at 100 should drop to 50, proving the amount is
        // configured rather than always full.
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer_with_slash(dp, 0.5);

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_double_sign_partial";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

        let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
            let token = entry.value();
            token.blockchain.get_current_chain_state().last_hash_in
        } else {
            vec![42u8; 32]
        };

        // Same proposer — double-signed scenario
        committer.stake_store.add_staker(vec![10], 100);

        // Insert existing candidate with same proposer key
        let existing_block = make_block_with_proposer(&committer, "tx_existing", vec![10]);
        committer.candidate_registry.insert(
            vec![1], prev_hash.clone(), existing_block, vec![10],
        );

        let block = make_block_with_proposer(&committer, tx_id, vec![10]); // SAME proposer!
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, vec![10]).await;
        // AUDIT Phase 5.2 / H2: the double-signed re-proposal is rejected on the commit path.
        assert!(
            matches!(result, Err(CommitterError::LoserDiscarded)),
            "a double-signed re-proposal must be discarded on the commit path"
        );

        // The partial slash (0.5) is still applied even though the block is rejected.
        assert_eq!(
            committer.stake_store.get_stake(&vec![10]),
            50,
            "0.5 fraction of 100 should leave 50"
        );
        assert_eq!(
            committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
            0,
            "the double-signed candidate group must be cleared"
        );
    }

    #[tokio::test]
    async fn commit_no_existing_candidates_inserts_first() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_first";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

        let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
        let prev_hash = block.previous_hash.clone();
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        let result = committer.check_and_commit_transaction_results(&commit, vec![]).await;
        assert!(result.is_ok());

        // First candidate should be inserted into the registry
        assert_eq!(
            committer.candidate_registry.candidate_count(&vec![1], &prev_hash),
            1,
        );
    }

    // --- Conflict resolution: discard losers + bound the registry (AUDIT Phase 5.2 / H2) ---

    #[tokio::test]
    async fn commit_conflict_rolls_back_loser_tip_and_commits_winner() {
        // AUDIT Phase 5.2 / H2, design decision #1: when the losing proposal is the
        // current chain tip, committing the winner rolls that tip back (guarded on a hash
        // match) and appends the winner, so exactly one block remains at the position and
        // the tip advances to the winner's hash — the loser is never left appended.
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp);

        // Bootstrap token + genesis chain.
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        // The genesis tip is the (token_id, previous_hash) position both proposals fight over.
        let prev_tip = committer.tokens.get(&vec![1]).unwrap().value()
            .blockchain.get_current_chain_state().last_hash_in;

        // Two competing proposals at the same position (genesis tip), distinct proposers.
        // Build both BEFORE committing so each chains off the genesis tip (they are siblings).
        let tx_a = "tx_roll_a";
        let block_a = make_block_with_proposer(&committer, tx_a, b"alpha".to_vec());
        let tx_b = "tx_roll_b";
        let block_b = make_block_with_proposer(&committer, tx_b, b"beta".to_vec());
        assert_eq!(block_a.previous_hash, prev_tip, "block_a is a sibling of block_b");
        assert_eq!(block_b.previous_hash, prev_tip, "block_b is a sibling of block_a");

        // Give block_a the finalizing entry and commit it: it becomes the chain tip AND is
        // recorded as the first candidate at prev_tip.
        make_finalizing_entry(&registry, tx_a, b"alice".to_vec());
        committer.stake_store.add_staker(b"alpha".to_vec(), 100);
        let commit_a = TransactionCommit {
            trans_id: tx_a.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block_a,
        };
        assert!(committer.check_and_commit_transaction_results(&commit_a, b"alpha".to_vec()).await.is_ok());
        let tip_after_a = committer.tokens.get(&vec![1]).unwrap().value()
            .blockchain.get_current_chain_state().last_hash_in;
        assert_eq!(tip_after_a, commit_a.proposed_block.current_hash, "block_a is now the tip");

        // Now block_b (higher stake) is proposed at the same position.
        make_finalizing_entry(&registry, tx_b, b"alice".to_vec());
        committer.stake_store.add_staker(b"beta".to_vec(), 500);
        let commit_b = TransactionCommit {
            trans_id: tx_b.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block_b,
        };
        let result = committer.check_and_commit_transaction_results(&commit_b, b"beta".to_vec()).await;
        assert!(result.is_ok(), "higher-stake block_b wins the conflict and commits");

        // The loser (block_a) that was the tip is rolled back; block_b is the sole block at
        // the position; the candidate group is cleared.
        let tip = committer.tokens.get(&vec![1]).unwrap().value()
            .blockchain.get_current_chain_state().last_hash_in;
        assert_eq!(tip, commit_b.proposed_block.current_hash,
            "the winner's hash is the new tip (the rolled-back loser is gone)");
        assert_eq!(
            committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
            2,
            "genesis + winner: the rolled-back loser leaves exactly one appended block"
        );
        assert_eq!(
            committer.candidate_registry.candidate_count(&vec![1], &prev_tip),
            0,
            "the resolved candidate group must be cleared"
        );
    }

    #[tokio::test]
    async fn commit_conflict_rejects_losing_commit() {
        // AUDIT Phase 5.2 / H2: when the incoming block LOSES its conflict it is rejected
        // with `LoserDiscarded`, the existing (winning) block stays as the tip, and the
        // resolved candidate group is cleared — the losing commit never reaches the chain.
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp);

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        // genesis tip is the contested position.
        let prev_tip = committer.tokens.get(&vec![1]).unwrap().value()
            .blockchain.get_current_chain_state().last_hash_in;

        let tx_a = "tx_rej_a";
        let tx_b = "tx_rej_b";
        let block_a = make_block_with_proposer(&committer, tx_a, b"alpha".to_vec());
        let block_b = make_block_with_proposer(&committer, tx_b, b"beta".to_vec());
        assert_eq!(block_a.previous_hash, prev_tip);
        assert_eq!(block_b.previous_hash, prev_tip);

        // block_a: HIGH stake (it will win); block_b: LOW stake (it will lose).
        make_finalizing_entry(&registry, tx_a, b"alice".to_vec());
        committer.stake_store.add_staker(b"alpha".to_vec(), 500);
        let commit_a = TransactionCommit {
            trans_id: tx_a.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block_a,
        };
        assert!(committer.check_and_commit_transaction_results(&commit_a, b"alpha".to_vec()).await.is_ok());
        let tip_after_a = committer.tokens.get(&vec![1]).unwrap().value()
            .blockchain.get_current_chain_state().last_hash_in;
        assert_eq!(tip_after_a, commit_a.proposed_block.current_hash);

        // block_b (lower stake) is the loser — it must be rejected, not appended.
        make_finalizing_entry(&registry, tx_b, b"alice".to_vec());
        committer.stake_store.add_staker(b"beta".to_vec(), 100);
        let commit_b = TransactionCommit {
            trans_id: tx_b.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block_b,
        };
        let result = committer.check_and_commit_transaction_results(&commit_b, b"beta".to_vec()).await;
        assert!(
            matches!(result, Err(CommitterError::LoserDiscarded)),
            "a lower-stake losing commit must be discarded on the commit path"
        );

        // The tip is unchanged (block_a stays); the loser was never appended; the group cleared.
        let tip = committer.tokens.get(&vec![1]).unwrap().value()
            .blockchain.get_current_chain_state().last_hash_in;
        assert_eq!(tip, commit_a.proposed_block.current_hash, "block_a remains the tip");
        assert_eq!(
            committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count(),
            2,
            "only block_a is appended; the losing commit never reaches the chain"
        );
        assert_eq!(
            committer.candidate_registry.candidate_count(&vec![1], &prev_tip),
            0,
            "the resolved candidate group must be cleared"
        );
    }

    #[tokio::test]
    async fn commit_conflict_uses_verified_proposer_not_forged_key() {
        // AUDIT Phase 5.8 / M10 discriminator: the incoming block's self-declared
        // `proposer_key` claims a high-stake identity (beta, 500) that the signed envelope
        // does NOT actually hold — the authenticated sender is delta (unregistered, stake 0).
        // Resolution must key off the *verified* sender (delta), never the unsigned self-
        // declared key, so a forged high-stake proposer_key can no longer steer the branch.
        //
        // On code that trusts self-declared proposer_key: beta (500) beats alpha (100) → the
        // forged block commits (is_ok): the attacker steers the branch. On fixed code: the
        // verified sender delta (0) loses to alpha (100) → the forged block is rejected.
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp);

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_forged_key";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

        let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
            let token = entry.value();
            token.blockchain.get_current_chain_state().last_hash_in
        } else {
            vec![42u8; 32]
        };

        // alpha: the real, winning candidate (stake 100). beta: a forged self-declared
        // identity the incoming claims (stake 500) that no verified sender holds.
        committer.stake_store.add_staker(b"alpha".to_vec(), 100);
        committer.stake_store.add_staker(b"beta".to_vec(), 500);

        let existing_block = make_block_with_proposer(&committer, "tx_existing", b"alpha".to_vec());
        committer.candidate_registry.insert(
            vec![1], prev_hash.clone(), existing_block, b"alpha".to_vec(),
        );

        // Incoming self-declared proposer = beta (forged, 500); verified sender (finalizer_key)
        // = delta — not registered, so stake 0.
        let block = make_block_with_proposer(&committer, tx_id, b"beta".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        // Verified proposer (delta, 0) loses to alpha (100) → incoming rejected.
        let result = committer.check_and_commit_transaction_results(&commit, b"delta".to_vec()).await;
        assert!(
            matches!(result, Err(CommitterError::LoserDiscarded)),
            "a forged high-stake proposer_key (beta) must not steer resolution; the incoming (verified delta) is rejected"
        );
    }

    #[tokio::test]
    async fn commit_conflict_folds_over_all_candidates() {
        // AUDIT Phase 5.8 / M10 discriminator: two competing candidates sit at one position.
        // candidates[0] = alpha (stake 100), candidates[1] = beta (stake 500); the incoming
        // (verified delta) has stake 300 — it beats alpha but loses to beta.
        //
        // On code that compares only candidates[0]: delta (300) beats alpha (100) → commits.
        // On fixed code (fold over ALL candidates): delta loses to beta (500) → rejected.
        // Order is deterministic here because the CandidateRegistry stores candidates in
        // insertion order (push), so alpha is candidates[0] and beta is candidates[1].
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp);

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_fold";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

        let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
            let token = entry.value();
            token.blockchain.get_current_chain_state().last_hash_in
        } else {
            vec![42u8; 32]
        };

        committer.stake_store.add_staker(b"alpha".to_vec(), 100);
        committer.stake_store.add_staker(b"beta".to_vec(), 500);
        committer.stake_store.add_staker(b"delta".to_vec(), 300);

        // Insert alpha (weak) first, then beta (strong): candidates[0]=alpha, candidates[1]=beta.
        let c0 = make_block_with_proposer(&committer, "tx_c0", b"alpha".to_vec());
        let c1 = make_block_with_proposer(&committer, "tx_c1", b"beta".to_vec());
        committer.candidate_registry.insert(vec![1], prev_hash.clone(), c0, b"alpha".to_vec());
        committer.candidate_registry.insert(vec![1], prev_hash.clone(), c1, b"beta".to_vec());

        // Incoming verified sender = delta (300).
        let block = make_block_with_proposer(&committer, tx_id, b"delta".to_vec());
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        // Fold: delta beats alpha but loses to beta → rejected (LoserDiscarded).
        let result = committer.check_and_commit_transaction_results(&commit, b"delta".to_vec()).await;
        assert!(
            matches!(result, Err(CommitterError::LoserDiscarded)),
            "incoming must be rejected: it loses to candidates[1] (beta) even though it beats candidates[0] (alpha)"
        );
    }

    #[tokio::test]
    async fn candidate_registry_bounded_under_repeated_conflicts() {
        // AUDIT Phase 5.2 / H2: repeatedly populating one (token_id, previous_hash) position
        // with competing proposals (the shape a sustained conflict/storm produces) must not let
        // the candidate group grow without bound. The CandidateRegistry caps each position at
        // DEFAULT_MAX_CANDIDATES via LRU eviction (oldest evicted). The commit path inserts
        // (no-conflict branch) and reads (conflict branch) through this same primitive, so a
        // bounded registry bounds the conflict surface at commit time. This drives `insert`
        // directly to exercise the cap; see `commit_conflict_rolls_back_loser_tip_and_commits_winner`
        // and `commit_conflict_rejects_losing_commit` for the resolved-group-cleared behavior on
        // the actual commit path.
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _, _) = make_test_committer(dp);

        // Bootstrap a token so the position is realistic.
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);
        let prev_hash = committer.tokens.get(&vec![1]).unwrap().value()
            .blockchain.get_current_chain_state().last_hash_in;

        // Insert far more competing proposals at this one position than the cap allows.
        const N: usize = CandidateRegistry::DEFAULT_MAX_CANDIDATES + 16;
        for i in 0..N {
            let block = make_block_with_proposer(&committer, &format!("tx_bnd_{i}"), vec![i as u8]);
            committer.candidate_registry.insert(vec![1], prev_hash.clone(), block, vec![i as u8]);
        }

        // The position is capped at DEFAULT_MAX_CANDIDATES, and the oldest 16 were evicted (LRU):
        // each of tx_bnd_0..tx_bnd_15 is absent from the survivors, while the newest ones remain.
        for i in 0..16 {
            let present = committer.candidate_registry.get_candidates(&vec![1], &prev_hash)
                .iter()
                .any(|(block, _)| block.signed_trans.transaction_id == format!("tx_bnd_{i}"));
            assert!(!present, "oldest proposal tx_bnd_{i} must be evicted under LRU eviction");
        }
    }

    // --- Payload-match + block_hash registry (AUDIT Phase 3.5 / H12) ---

    #[tokio::test]
    async fn check_and_commit_rejects_payload_mismatch() {
        // Headline H12 discriminator. A commit whose block embeds a transaction differing from the
        // validated/pooled one must be rejected — without the payload-match gate the Committer would
        // happily append whatever block arrived on the wire.
        let dp = Arc::new(TestDataProvider::new());
        dp.insert_user(b"alice".to_vec(), "token".to_string(), User {
            public_key: b"alice".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        });
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_payload_mismatch";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());

        // Build a matching block, then tamper its embedded transaction (swap receiver) so the wire
        // payload differs from the validated entry the Committer holds.
        let mut block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
        block.signed_trans.transaction.receiver = vec![255];

        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        match committer.check_and_commit_transaction_results(&commit, vec![]).await {
            Err(CommitterError::TransactionPayloadMismatch(_)) => {}
            other => panic!("expected TransactionPayloadMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn committed_transaction_records_block_hash_not_token_id() {
        // Misnomer discriminator (H12). Committed.block_hash must record the hash of the block the
        // transaction was committed *into*, never the token id (which is what the pre-fix Committer
        // stored). We pin the entry with a second lock so it survives the commit flow (which would
        // otherwise remove it once lock_count hits 0) and inspect its persisted state.
        let dp = Arc::new(TestDataProvider::new());
        let (committer, registry, _logger) = make_test_committer(dp.clone());

        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let tx_id = "tx_blockhash";
        make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
        {
            let mut entry = registry.get_transaction_mut(tx_id).unwrap();
            entry.acquire().unwrap();
        }

        let block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
        let committed_hash = block.current_hash.clone();
        let commit = TransactionCommit {
            trans_id: tx_id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test".to_string(),
            proposed_block: block,
        };

        assert!(committer.check_and_commit_transaction_results(&commit, vec![]).await.is_ok());

        // The entry persists (pinned by the second lock); read back its Committed.block_hash.
        let entry = registry.get_transaction_mut(tx_id).unwrap();
        match &entry.state {
            TransactionState::Committed { transaction, block_hash } => {
                assert_eq!(block_hash, &committed_hash);
                assert_ne!(block_hash, &vec![1]); // not the token id
                assert_eq!(&transaction.sender, &b"alice".to_vec());
            }
            other => panic!("expected Committed, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Block gossip tests
    // -----------------------------------------------------------------------

    /// Build a valid block for the given transaction, chained off the current tip.
    /// Build a gossip block that chains off the token's **live** chain tip, carrying a valid,
    /// self-consistent finalizer signature (AUDIT Phase 3.3 / C5). `verify_block_finalizer_sig`
    /// re-checks this signature in `handle_block_finalized`, so an empty or forged signature would
    /// now be rejected — a pre-fix `make_gossip_block` used `signature: vec![]` and slipped through.
    fn make_gossip_block(committer: &Committer, trans_id: &str, proposer_key: Vec<u8>) -> Block {
        let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
            entry.value().blockchain.get_current_chain_state().last_hash_in
        } else {
            vec![42u8; 32]
        };
        make_gossip_block_at_prev(trans_id, proposer_key, &prev_hash)
    }

    /// Build a gossip block with a caller-supplied `previous_hash` (so several blocks can share a
    /// frozen parent, as in a sibling-race test) and a valid finalizer signature. Distinct
    /// `trans_id`s yield distinct `transaction_hash`/`signature` values, hence distinct
    /// `current_hash` — exactly what a true sibling race needs. The finalizer signs the stored
    /// `transaction_hash` (which `create_hash` binds via `CanonicalSignedTransaction`), so the
    /// signature and the block hash are mutually consistent.
    fn make_gossip_block_at_prev(
        trans_id: &str,
        proposer_key: Vec<u8>,
        prev_hash: &[u8],
    ) -> Block {
        // A throwaway test finalizer key. `finalizer_addr` is its public key; `signature` is an
        // Ed25519 sign over the stored `transaction_hash`, which `verify_block_finalizer_sig`
        // re-checks against `finalizer_addr`. Any fixed value works for `transaction_hash` — it
        // only has to survive inside the canonical bytes that `create_hash` hashes.
        let finalizer = Ed25519Provider::generate();
        let finalizer_addr = finalizer.public_key().expect("finalizer public key");
        let transaction_hash = format!("gossip-{trans_id}").into_bytes();
        let signature = finalizer.sign_data(&transaction_hash).expect("finalizer signature");

        let signed = SignedTransaction {
            transaction_id: trans_id.to_string(),
            transaction: Transaction {
                id: trans_id.to_string(),
                action: "Process".into(),
                token_id: vec![1],
                bid: None,
                sequence_number: 1,
                sender: b"alice".to_vec(),
                receiver: b"bob".to_vec(),
                amount: Some(100),
                timestamp: 0,
                result_hash: vec![],
                sender_signature: vec![],
            },
            total_voters: 3,
            total_stake: 42,
            leader_hash: prev_hash.to_vec(),
            leader_address: vec![],
            leader_stake: 0,
            finalizer_addr: finalizer_addr.clone(),
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: transaction_hash.clone(),
                signature,
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: proposer_key,
        };

        let mut block = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash.to_vec(),
            current_hash: vec![],
            timestamp: 0,
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        block.current_hash = BlockFactory::create_hash(&block);
        block
    }

    /// Build a wire Message for a BlockFinalized gossip event.
    fn make_block_finalized_message(block: Block) -> Message {
        let body = serialize_to_bytes_rmp(&block).expect("Block serialization");
        Message {
            chain_id: "test".to_string(),
            action: String::from("BlockFinalized"),
            body,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        }
    }

    #[tokio::test]
    async fn concurrent_block_finalized_submissions_no_panic() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let original_chain_len = committer.tokens.get(&vec![1])
            .unwrap()
            .value()
            .blockchain
            .get_count();

        // Spawn multiple concurrent handle_block_finalized calls with different tx_ids
        let committer_arc = Arc::new(committer);
        std::thread::scope(|s| {
            let mut handles = vec![];
            for i in 0..10 {
                let committer = committer_arc.clone();
                let tx_id = format!("concurrent_tx_{}", i);
                handles.push(s.spawn(move || {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    let block = make_gossip_block(&committer, &tx_id, vec![i as u8]);
                    let message = make_block_finalized_message(block);
                    // Each thread runs its own runtime to handle the async call
                    rt.block_on(async {
                        committer.handle_block_finalized(message).await
                    })
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });

        // All joins succeeded → no data races
        // Chain should have grown (some blocks may have been orphaned due to concurrent writes)
        let new_chain_len = committer_arc.tokens.get(&vec![1])
            .unwrap()
            .value()
            .blockchain
            .get_count();
        assert!(new_chain_len >= original_chain_len);
    }

    #[tokio::test]
    async fn handle_block_finalized_appends_valid_block() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // Bootstrap token and create a genesis block
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let original_chain_len = committer.tokens.get(&vec![1])
            .unwrap()
            .value()
            .blockchain
            .get_count();
        assert_eq!(original_chain_len, 1); // genesis only

        // Build a valid next block chained off the current tip
        let block = make_gossip_block(&committer, "gossip_tx", b"mallory".to_vec());
        let message = make_block_finalized_message(block);

        // Should succeed and append the block
        let result = committer.handle_block_finalized(message).await;
        assert!(result.is_ok());

        // Chain should have grown by 1
        let new_chain_len = committer.tokens.get(&vec![1])
            .unwrap()
            .value()
            .blockchain
            .get_count();
        assert_eq!(new_chain_len, original_chain_len + 1);
    }

    /// (AUDIT Phase 3.3 / C5 discriminator) N concurrent sibling blocks — same frozen parent
    /// (`previous_hash`), distinct tx_ids ⇒ distinct `current_hash`, each carrying a valid finalizer
    /// signature — may only append ONE. The atomic `append_validated_block` (a single `get_mut`
    /// spanning the tip read and the push) guarantees this; without it, the read-then-`get_mut`
    /// gap let two siblings both validate and append (a fork), growing the chain by more than one.
    #[tokio::test]
    async fn concurrent_sibling_blocks_exactly_one_appended() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain.
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        // Capture the tip once; every sibling chains off this same parent.
        let tip = committer.tokens.get(&vec![1]).unwrap().value().blockchain
            .get_current_chain_state().last_hash_in;
        let original_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
        assert_eq!(original_len, 1); // genesis only

        // Build N sibling blocks up front, each with the frozen `previous_hash` and its own valid
        // finalizer signature — distinct tx_ids make their `current_hash` distinct.
        let n = 16;
        let siblings: Vec<Block> = (0..n)
            .map(|i| make_gossip_block_at_prev(&format!("sibling-{i}"), vec![i as u8], &tip))
            .collect();

        // Fan out the concurrent handlers; each runs its own runtime.
        // A single runtime shared across all handler threads: each spawned handler drives to
        // completion on its own OS thread, so the handlers actually race in wall-clock time — which
        // exposes the read-then-get_mut gap (with the gap, every sibling reads the same stale tip
        // and all of them append). A per-thread `Runtime::new()` serialized startup and hid the race.
        let committer_arc = Arc::new(committer);
        // Pre-create each handler's runtime up front, outside the spawn loop, so all N handlers
        // begin their block_on in the same instant. The per-thread `Runtime::new()` inside the
        // loop previously serialized startup and hid the race. (A shared multi_thread runtime
        // can't be dropped off the worker threads here, so each thread keeps its own current-thread
        // runtime and drops it on its own OS thread.)
        let runtimes: Vec<_> = (0..n).map(|_| tokio::runtime::Runtime::new().unwrap()).collect();
        std::thread::scope(|s| {
            let mut handles = vec![];
            for (block, rt) in siblings.into_iter().zip(runtimes) {
                let committer = committer_arc.clone();
                let message = make_block_finalized_message(block);
                handles.push(s.spawn(move || {
                    rt.block_on(async { committer.handle_block_finalized(message).await })
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });

        // Exactly one of the N siblings appended; the rest were rejected with LinkageMismatch.
        let new_len = committer_arc.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
        assert_eq!(new_len, original_len + 1);
    }

    /// (AUDIT Phase 3.3 / C5 discriminator) A block whose finalizer signature does not verify is
    /// rejected (`Err(InvalidFinalizerSignature)`) and never appended. A pre-fix block with an
    /// empty `finalizer_sig` would have been accepted and appended.
    #[tokio::test]
    async fn handle_block_finalized_rejects_bad_finalizer_sig() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain.
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        // A validly-chained block (correct previous_hash, valid finalizer sig) — then forge the
        // signature bytes so verification fails.
        let mut block = make_gossip_block(&committer, "forged", b"mallory".to_vec());
        let original_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
        block.signed_trans.finalizer_sig.signature = vec![0xAA; 64]; // forged

        let message = make_block_finalized_message(block);
        let result = committer.handle_block_finalized(message).await;
        assert!(matches!(result, Err(CommitterError::InvalidFinalizerSignature)));

        // Rejected → nothing appended.
        let chain_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
        assert_eq!(chain_len, original_len);
    }

    /// (AUDIT Phase 5.5 / H13 discriminator) A `DistributeToken` carrying an id already present in
    /// the local cache is refused and leaves the cached token — chain and metadata — untouched. The
    /// old `self.tokens.insert` blindly overwrote the existing token, so any peer could swap in an
    /// arbitrary chain/metadata under a settled id. Proven discriminator: restoring the blind insert
    /// makes the handler return `Ok(())` and overwrites `name`/`asset_hash` (all value assertions
    /// fail without the fix).
    #[tokio::test]
    async fn handle_token_distribution_rejects_conflicting_token_id() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, collector) = make_test_committer(dp);

        // Bootstrap the authoritative token: id vec![1], distinct metadata + asset_hash, and a seeded
        // genesis chain. Pin the tip and length so we can prove the cached chain survives.
        let mut original = Token::new();
        original.id = vec![1];
        original.set_metadata("name".to_string(), "original".to_string());
        original.asset_hash = vec![0x01u8; 32];
        committer.bootstrap_token(original);
        bootstrap_token_chain(&committer);
        let before_tip = committer
            .tokens
            .get(&vec![1])
            .unwrap()
            .value()
            .blockchain
            .get_current_chain_state()
            .last_hash_in;
        let before_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();

        // A malicious token carries the SAME id but different metadata and asset_hash — a peer trying
        // to swap in an alternative token under the settled id.
        let mut malicious = Token::new();
        malicious.id = vec![1];
        malicious.set_metadata("name".to_string(), "swapped".to_string());
        malicious.asset_hash = vec![0xFFu8; 32];

        let body = serialize_to_bytes_rmp(&malicious).expect("Token serialization");
        let message = Message {
            chain_id: "test".to_string(),
            action: String::from("DistributeToken"),
            body,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };
        let result = committer.handle_token_distribution(message).await;

        // The conflicting distribution is refused, and every part of the cached token is preserved.
        assert!(matches!(result, Err(CommitterError::TokenConflict(_))));
        let cached_ref = committer.tokens.get(&vec![1]).unwrap();
        let cached = cached_ref.value();
        assert_eq!(cached.metadata.get("name").map(String::as_str), Some("original"));
        assert_eq!(cached.asset_hash, vec![0x01u8; 32]);
        assert_eq!(
            cached.blockchain.get_current_chain_state().last_hash_in,
            before_tip
        );
        assert_eq!(cached.blockchain.get_count(), before_len);
        // The rejection is logged, not silent.
        assert!(collector
            .logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("TOKEN REPLACEMENT REJECTED")));
    }

    /// (AUDIT Phase 5.5 / H13 positive guard) A `DistributeToken` whose id is not already cached is
    /// accepted — this is how a node joining the network seeds a token it lacks. Guards against the
    /// fix over-rejecting the legitimate seeding path.
    #[tokio::test]
    async fn handle_token_distribution_accepts_new_token_id() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // A pre-existing token under id vec![1].
        let mut existing = Token::new();
        existing.id = vec![1];
        existing.set_metadata("name".to_string(), "existing".to_string());
        committer.bootstrap_token(existing);

        // A distribution carrying a brand-new id vec![9].
        let mut newcomer = Token::new();
        newcomer.id = vec![9];
        newcomer.set_metadata("name".to_string(), "newcomer".to_string());

        let body = serialize_to_bytes_rmp(&newcomer).expect("Token serialization");
        let message = Message {
            chain_id: "test".to_string(),
            action: String::from("DistributeToken"),
            body,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };
        let result = committer.handle_token_distribution(message).await;

        assert!(result.is_ok());
        let cached_ref = committer
            .tokens
            .get(&vec![9])
            .expect("newcomer token should be cached");
        let cached = cached_ref.value();
        assert_eq!(cached.metadata.get("name").map(String::as_str), Some("newcomer"));
    }

    /// (AUDIT Phase 3.4 / H15 discriminator) Out-of-order delivery is buffered, not dropped, and
    /// all blocks eventually commit. Deliver the second block (`b2`) before its parent `b1`: `b2` is
    /// buffered (chain length unchanged), and when `b1` lands the buffer is replayed and `b2` is
    /// promoted. Proven discriminator: restoring the old silent-drop behavior yields a chain length
    /// of `original + 1` after `b1` (`b2` lost) — the assertion fails without the fix.
    #[tokio::test]
    async fn handle_block_finalized_buffers_orphan_and_replays_on_tip_advance() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain (genesis only).
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let original_chain_len = committer.tokens.get(&vec![1])
            .unwrap()
            .value()
            .blockchain
            .get_count();
        assert_eq!(original_chain_len, 1); // genesis only

        // The current tip (genesis). b1 chains off it; b2 chains off b1.
        let tip = committer.tokens.get(&vec![1]).unwrap().value().blockchain
            .get_current_chain_state().last_hash_in;
        let b1 = make_gossip_block_at_prev("orphan-b1", b"proposer-1".to_vec(), &tip);
        let b2 = make_gossip_block_at_prev("orphan-b2", b"proposer-2".to_vec(), &b1.current_hash);

        // Deliver b2 FIRST (its parent b1 has not landed) → buffered, not appended.
        let result = committer.handle_block_finalized(make_block_finalized_message(b2)).await;
        assert!(result.is_ok(), "buffering an orphan is non-fatal");
        let len_after_b2 = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
        assert_eq!(len_after_b2, original_chain_len, "orphan b2 not appended before its parent");
        assert_eq!(committer.orphan_blocks.lock().await.len(), 1, "b2 is buffered");

        // Now deliver b1 → it appends, and the replay loop promotes b2 whose parent is now the tip.
        let result = committer.handle_block_finalized(make_block_finalized_message(b1)).await;
        assert!(result.is_ok());
        let len_after_b1 = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
        assert_eq!(len_after_b1, original_chain_len + 2, "b1 appends and b2 is promoted");
        assert!(committer.orphan_blocks.lock().await.is_empty(), "b2 promoted out of the buffer");
    }

    /// (AUDIT Phase 3.4 / H15 — cascade + reorder) A chain delivered in adversarially out-of-order
    /// order all eventually commits via cascading replay. Order `[b3, b5, b1, b4, b2]`: early
    /// blocks are buffered; each subsequent real block triggers a cascade that promotes everything
    /// that now chains onto the advancing tip. A non-cascading replay (promote only the one block
    /// whose parent is the tip) would stop after `b3` — the cascade is what drives `b4`, `b5` home.
    #[tokio::test]
    async fn handle_block_finalized_replays_orphan_cascade_in_out_of_order_delivery() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain (genesis only).
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let original_chain_len = committer.tokens.get(&vec![1])
            .unwrap()
            .value()
            .blockchain
            .get_count();
        assert_eq!(original_chain_len, 1);

        // Build a 5-block chain up front, each chaining off the prior block's hash.
        let mut tip = committer.tokens.get(&vec![1]).unwrap().value().blockchain
            .get_current_chain_state().last_hash_in;
        let mut chain = Vec::new();
        for i in 0..5 {
            let b = make_gossip_block_at_prev(&format!("cascade-{i}"), vec![i as u8], &tip);
            tip = b.current_hash.clone();
            chain.push(b);
        }

        // Deliver in adversarial order: the chain breaks at b1/b2, so b3 and b5 buffer first, then
        // b4 cannot chain until its parent b3 lands, etc.
        let order = [2usize, 4, 0, 3, 1]; // b3, b5, b1, b4, b2
        for &i in &order {
            let result = committer.handle_block_finalized(make_block_finalized_message(chain[i].clone())).await;
            assert!(result.is_ok(), "block {i} delivered without error");
        }

        // All five landed: a single chain of genesis + 5.
        let final_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
        assert_eq!(final_len, original_chain_len + 5);
        assert!(committer.orphan_blocks.lock().await.is_empty(), "everything promoted out");

        // The chain is now contiguous: each block's previous_hash matches its predecessor's hash.
        let entry = committer.tokens.get(&vec![1]).unwrap();
        let blocks: Vec<&Block> = entry.value().blockchain.chain.iter().collect();
        let mut idx = 1;
        while idx < blocks.len() {
            assert_eq!(blocks[idx].previous_hash, blocks[idx - 1].current_hash);
            idx += 1;
        }
    }

    #[tokio::test]
    async fn handle_block_finalized_rejects_tampered_block() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        // Build a validly-chained block, then tamper the current_hash
        let block = make_gossip_block(&committer, "tampered", b"tampered".to_vec());
        // Tamper the hash so validation fails
        let block = Block {
            signed_trans: block.signed_trans,
            token_metadata: block.token_metadata,
            previous_hash: block.previous_hash,
            current_hash: vec![0xAA, 0xBB, 0xCC], // tampered
            timestamp: block.timestamp,
            finality_status: block.finality_status,
            proposer_key: block.proposer_key,
            epoch_number: block.epoch_number,
        };

        let message = make_block_finalized_message(block);

        let result = committer.handle_block_finalized(message).await;
        assert!(matches!(result, Err(CommitterError::InvalidBlockHash)));
    }

    #[tokio::test]
    async fn handle_block_finalized_unknown_token_returns_error() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // A valid finalizer signature so the block clears the C5 gate and reaches the expected
        // TokenNotFound path (this block targets a token that is not in the committer's cache).
        let unknown_finalizer = Ed25519Provider::generate();
        let unknown_finalizer_addr = unknown_finalizer.public_key().expect("finalizer public key");
        let unknown_tx_hash = b"unknown-transaction-hash".to_vec();
        let unknown_sig = unknown_finalizer.sign_data(&unknown_tx_hash).expect("finalizer signature");
        let signed = SignedTransaction {
            transaction_id: "unknown".to_string(),
            transaction: Transaction {
                id: "unknown".to_string(),
                action: "Process".into(),
                token_id: vec![99], // not in committer's token cache
                bid: None,
                sequence_number: 1,
                sender: b"alice".to_vec(),
                receiver: b"bob".to_vec(),
                amount: Some(100),
                timestamp: 0,
                result_hash: vec![],
                sender_signature: vec![],
            },
            total_voters: 3,
            total_stake: 42,
            leader_hash: vec![],
            leader_address: vec![],
            leader_stake: 0,
            finalizer_addr: unknown_finalizer_addr,
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: unknown_tx_hash,
                signature: unknown_sig,
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: b"unknown".to_vec(),
        };

        let block = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: vec![],
            current_hash: vec![0xDE, 0xAD],
            timestamp: 0,
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };

        let message = make_block_finalized_message(block);

        let result = committer.handle_block_finalized(message).await;
        assert!(matches!(result, Err(CommitterError::TokenNotFound(_))));
    }

    #[tokio::test]
    async fn handle_block_quorum_reached_updates_finality_status() {
        // Direct test of Blockchain::set_finality_status.
        // The handler's job is to find the block by hash and call this.
        let mut blockchain = pneumatic_core::blocks::Blockchain::new();

        // Build a block the same way the other tests do
        let block = make_test_block_for_token_internal(&blockchain);
        let block_hash = block.current_hash.clone();
        blockchain.add_block(block);

        // Verify initial status
        let tip = blockchain.get_block_at(0).unwrap();
        assert_eq!(tip.finality_status, FinalityStatus::Optimistic);

        // Transition to Confirmed
        let result = blockchain.set_finality_status(&block_hash, FinalityStatus::Confirmed);
        assert!(result.is_ok());

        // Verify final status
        let tip = blockchain.get_block_at(0).unwrap();
        assert_eq!(tip.finality_status, FinalityStatus::Confirmed);
    }

    /// Helper to build a test block with a valid hash.
    fn make_test_block_for_token_internal(blockchain: &pneumatic_core::blocks::Blockchain) -> Block {
        let prev_hash: Vec<u8> = if blockchain.get_count() == 0 {
            vec![42u8; 32]
        } else {
            blockchain.get_current_chain_state().last_hash_in
        };

        let signed = SignedTransaction {
            transaction_id: "test".to_string(),
            transaction: Transaction {
                id: "test".to_string(),
                action: "Process".into(),
                token_id: vec![1],
                bid: None,
                sequence_number: 1,
                sender: b"alice".to_vec(),
                receiver: b"bob".to_vec(),
                amount: Some(100),
                timestamp: 0,
                result_hash: vec![],
                sender_signature: vec![],
            },
            total_voters: 3,
            total_stake: 42,
            leader_hash: prev_hash.clone(),
            leader_address: vec![],
            leader_stake: 0,
            finalizer_addr: vec![],
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![],
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: vec![],
        };

        let mut block = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        block.current_hash = BlockFactory::create_hash(&block);
        block
    }

    #[tokio::test]
    async fn handle_block_confirmed_vote_skips_missing_stake_set() {
        // When a vote arrives before BlockFinalized, it should be silently ignored
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // No BlockFinalized was received, so no stake set is cached
        let body = serialize_to_bytes_rmp(&(vec![1, 2, 3], vec![4, 5, 6]))
            .expect("Vote serialization");
        let message = Message {
            chain_id: "test".to_string(),
            action: String::from("BlockConfirmed"),
            body,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };

        // Should not error, just ignore
        let result = committer.handle_block_confirmed_vote(message).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn handle_block_confirmed_vote_accumulates_stake() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        // Build block and cache stake set
        let block = make_gossip_block(&committer, "vote_test", b"vote".to_vec());
        let block_hash = block.current_hash.clone();

        // Cache a stake set: alice=100, bob=50, charlie=50 (total=200)
        let mut stake_set = StakeSet::default();
        stake_set.stakers.insert(b"alice".to_vec(), 100);
        stake_set.stakers.insert(b"bob".to_vec(), 50);
        stake_set.stakers.insert(b"charlie".to_vec(), 50);

        // Manually cache the stake set
        committer.stake_set_cache.lock().await
            .insert(block_hash.clone(), stake_set.clone());

        // First vote: alice (stake=100, should be below 67% quorum of 200 = 134)
        let body1 = serialize_to_bytes_rmp(&(block_hash.clone(), b"alice".to_vec()))
            .expect("Vote serialization");
        let msg1 = Message {
            chain_id: "test".to_string(),
            action: String::from("BlockConfirmed"),
            body: body1,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };
        let result = committer.handle_block_confirmed_vote(msg1).await;
        assert!(result.is_ok());

        // Second vote: bob (cumulative=150, should cross quorum threshold)
        let body2 = serialize_to_bytes_rmp(&(block_hash.clone(), b"bob".to_vec()))
            .expect("Vote serialization");
        let msg2 = Message {
            chain_id: "test".to_string(),
            action: String::from("BlockConfirmed"),
            body: body2,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };
        let result = committer.handle_block_confirmed_vote(msg2).await;
        assert!(result.is_ok());

        // Verify cumulative stake reached quorum (150 >= 134)
        let votes = committer.confirmation_votes.lock().await;
        let (keys, cumulative) = votes.get(&block_hash).expect("block_hash should have votes");
        assert_eq!(keys.len(), 2);
        assert_eq!(*cumulative, 150);
    }

    // -----------------------------------------------------------------------
    // Phase 1.1 regression: outbound broadcasts signed with node identity
    // -----------------------------------------------------------------------

    /// A Connection that records each sent payload verbatim.
    struct RecordingConnection {
        recorder: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait::async_trait]
    impl pneumatic_core::conns::Connection for RecordingConnection {
        async fn send(&self, data: &Vec<u8>) -> Result<(), pneumatic_core::conns::ConnError> {
            self.recorder.lock().unwrap().push(data.clone());
            Ok(())
        }
    }

    /// Assert the message envelope is signed by `identity` — the same check
    /// the gossiper performs: signature over `body` under `public_key`.
    fn assert_signed_by(message: &Message, identity: &pneumatic_core::rns::identity::NodeIdentity) {
        let expected_pk = identity.ed25519.public_key().expect("identity pubkey");
        assert_eq!(
            message.public_key, expected_pk,
            "message.public_key must be the sender's identity key"
        );
        let verifier = pneumatic_core::crypto::Ed25519Provider::generate();
        let ok = verifier
            .check_signature(&message.signature, &message.public_key, &message.body)
            .expect("signature check should succeed");
        assert!(ok, "message body must verify under the sender's identity key");
    }

    /// Drive the public `handle_block_finalized` path and assert every
    /// outbound broadcast — the `BlockConfirmed` vote and the
    /// `DistributeBlock` payload — verifies under the committer identity.
    #[tokio::test]
    async fn block_finalized_broadcasts_signed_with_committer_identity() {
        let dp = Arc::new(TestDataProvider::new());
        let (committer, _registry, _logger) = make_test_committer(dp);

        // Bootstrap token and chain
        let mut token = Token::new();
        token.id = vec![1];
        committer.bootstrap_token(token);
        bootstrap_token_chain(&committer);

        let sentinel_recorder = Arc::new(Mutex::new(Vec::new()));
        let archiver_recorder = Arc::new(Mutex::new(Vec::new()));
        assert!(committer.node_registry.register_peer(
            vec![0xCC; 32],
            [3u8; 16],
            &NodeRegistryType::Sentinel,
            Box::new(RecordingConnection { recorder: sentinel_recorder.clone() }),
        ));
        assert!(committer.node_registry.register_peer(
            vec![0xAA; 32],
            [4u8; 16],
            &NodeRegistryType::Archiver,
            Box::new(RecordingConnection { recorder: archiver_recorder.clone() }),
        ));

        let block = make_gossip_block(&committer, "signed_tx", b"alice".to_vec());
        committer
            .handle_block_finalized(make_block_finalized_message(block))
            .await
            .expect("BlockFinalized should be accepted");

        // The sentinel received a BlockConfirmed vote signed by the committer.
        let vote_raw = sentinel_recorder
            .lock()
            .unwrap()
            .iter()
            .find(|raw| matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == "BlockConfirmed"))
            .cloned()
            .expect("sentinel should receive a BlockConfirmed vote");
        let vote: Message = deserialize_rmp_to(&vote_raw).expect("vote payload is a Message");
        assert_signed_by(&vote, &committer.identity);

        // The archiver received the DistributeBlock payload signed by the committer.
        let dist_raw = archiver_recorder
            .lock()
            .unwrap()
            .iter()
            .find(|raw| matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == "DistributeBlock"))
            .cloned()
            .expect("archiver should receive a DistributeBlock payload");
        let dist: Message = deserialize_rmp_to(&dist_raw).expect("dist payload is a Message");
        assert_signed_by(&dist, &committer.identity);
    }
}
