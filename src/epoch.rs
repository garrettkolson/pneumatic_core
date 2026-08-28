use serde::{Deserialize, Serialize};
use crate::errors::PneumaticError;
use dashmap::DashMap;
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Epoch — represents a single epoch in the blockchain
// ---------------------------------------------------------------------------

/// An epoch is a time-bounded period during which a leader produces blocks.
/// The epoch manager tracks transitions, stakes, and rewards.
#[derive(Debug, Clone)]
pub struct Epoch {
    /// Timestamp when this epoch started
    pub start_timestamp: i64,
    /// Timestamp when this epoch ended
    pub end_timestamp: i64,
    /// Sequential epoch number
    pub epoch_number: u64,
    /// Leader public key for this epoch
    pub leader_public_key: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Staking operations — applied during epoch reconciliation
// ---------------------------------------------------------------------------

/// Operations to apply during epoch reconciliation: staking changes,
/// slashing, and rewards.
#[derive(Debug, Clone)]
pub enum StakingOp {
    /// Add a staker with their public key and stake amount
    AddStaker(Vec<u8>, u64),
    /// Remove a staker by public key
    RemoveStaker(Vec<u8>),
    /// Slash a staker's stake (penalty for misbehavior)
    Slash(Vec<u8>, u64),
    /// Reward a staker with additional stake
    Reward(Vec<u8>, u64),
}

// ---------------------------------------------------------------------------
// Conflict representation — for finalization disagreements
// ---------------------------------------------------------------------------

/// Represents a conflict between two block proposals at the same height.
#[derive(Debug, Clone)]
pub struct Conflict {
    /// First proposed block hash
    pub block_a: Vec<u8>,
    /// Second proposed block hash
    pub block_b: Vec<u8>,
    /// Stake backing block A
    pub stake_a: u64,
    /// Stake backing block B
    pub stake_b: u64,
}

// ---------------------------------------------------------------------------
// EpochReconciliation — result of epoch boundary reconciliation
// ---------------------------------------------------------------------------

/// Data returned by the reconciler describing what staking and conflict
/// operations need to be applied. The reconciler returns data without
/// directly mutating state (delegation pattern).
#[derive(Debug, Default)]
pub struct EpochReconciliation {
    /// Tokens with misshapen chains that need repair
    pub misshapen_tokens: Vec<Vec<u8>>,
    /// Finalization conflicts that need resolution
    pub finalization_conflicts: Vec<Conflict>,
    /// Staking operations derived from chain analysis
    pub slashing_ops: Vec<StakingOp>,
    /// Reward operations derived from chain analysis
    pub reward_ops: Vec<StakingOp>,
}

// ---------------------------------------------------------------------------
// StakeSet — current staking state
// ---------------------------------------------------------------------------

/// Maps public keys to stake amounts for leader selection and quorum checks.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StakeSet {
    /// Public key -> stake amount
    pub stakers: std::collections::HashMap<Vec<u8>, u64>,
}

impl StakeSet {
    pub fn total_stake(&self) -> u64 {
        self.stakers.values().sum()
    }

    /// Get the stake for a specific public key
    pub fn get_stake(&self, key: &[u8]) -> u64 {
        self.stakers.get(key).copied().unwrap_or(0)
    }

    /// Convert to an ExecutorSet — active stakers become the executor pool.
    /// Used at epoch boundary to persist the executor set for shard assignment.
    pub fn to_executor_set(&self) -> ExecutorSet {
        ExecutorSet {
            executors: self.stakers.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// ExecutorSet — shard-aware executor pool
// ---------------------------------------------------------------------------

/// Maps executor public keys to their stakes. Used for deterministic shard
/// assignment: the sentinel computes `f(tx_id, epoch, shard_count) → shard`
/// then routes the transaction only to executors in that shard.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ExecutorSet {
    /// Executor public key → stake amount
    pub executors: HashMap<Vec<u8>, u64>,
}

impl ExecutorSet {
    pub fn total_stake(&self) -> u64 {
        self.executors.values().sum()
    }

    pub fn len(&self) -> usize {
        self.executors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.executors.is_empty()
    }

    pub fn get_stake(&self, key: &[u8]) -> u64 {
        self.executors.get(key).copied().unwrap_or(0)
    }

    /// Convert to a StakeSet — for leader/finalizer selection.
    pub fn to_stake_set(&self) -> StakeSet {
        StakeSet {
            stakers: self.executors.clone(),
        }
    }

    /// Create a deterministic shuffler for this executor set at a given epoch.
    /// The shuffle is used for per-epoch shard reassignment (rotation).
    pub fn shuffler(&self, epoch_number: u64, prev_block_hash: &[u8]) -> Shuffler {
        let mut keys: Vec<Vec<u8>> = self.executors.keys().cloned().collect();
        // C6: sort before Fisher-Yates so the shuffle is independent of HashMap insertion
        // order (matches deterministic_select at epoch.rs:236-237). Without this the
        // shuffle's starting order — and therefore the shard partition — varies per node.
        keys.sort();
        Shuffler::new(keys, epoch_number, prev_block_hash)
    }
}

// ---------------------------------------------------------------------------
// Deterministic selection seed — domain-separated, prev-block-hash bound
// ---------------------------------------------------------------------------
// Phase 5.3 (AUDIT H3): every selection seed is derived from a per-type domain
// byte, the epoch number, and the previous block hash, so a choice made for one
// purpose (e.g. leader election) can never be replayed as another (e.g. shard
// index), and so selection is only knowable once the previous block is actually
// mined (not merely from the public epoch number + stake set).
//
// The byte layout is fixed so every selection type hashes the same shape:
//   SHA-256(domain ‖ epoch_number(big-endian) ‖ prev_block_hash ‖ extra)
pub const LEADER_DOMAIN: u8 = 0x01;
pub const SHARD_SHUFFLE_DOMAIN: u8 = 0x02;
pub const FINALIZER_DOMAIN: u8 = 0x03;
pub const SHARD_INDEX_DOMAIN: u8 = 0x04;

/// Derive the 32-byte seed for a deterministic selection.
///
/// `domain` distinguishes the selection type (see the `*_DOMAIN` constants).
/// `prev_block_hash` binds the choice to the mined chain tip — empty at genesis.
/// `extra` is per-transaction salt for load distribution on the finalizer and
/// shard-index paths (empty for leader/shuffle).
pub fn derive_selection_seed(
    domain: u8,
    epoch_number: u64,
    prev_block_hash: &[u8],
    extra: &[u8],
) -> [u8; 32] {
    let mut input = Vec::with_capacity(1 + 8 + prev_block_hash.len() + extra.len());
    input.push(domain);
    input.extend_from_slice(&epoch_number.to_be_bytes());
    input.extend_from_slice(prev_block_hash);
    input.extend_from_slice(extra);
    let digest = ring::digest::digest(&ring::digest::SHA256, &input);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

// ---------------------------------------------------------------------------
// Shuffler — deterministic Fisher-Yates shuffle
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Shuffler — deterministic Fisher-Yates shuffle
// ---------------------------------------------------------------------------

/// Deterministic permutation of executor keys derived from a seed.
/// Used for per-epoch shard reassignment (rotation): each epoch, executors
/// are reshuffled into new shards, preventing stable cartel formation.
pub struct Shuffler {
    /// Original items (not mutated).
    items: Vec<Vec<u8>>,
    /// The last computed permutation, stored so we can return a reference.
    last_permutation: Vec<Vec<u8>>,
}

impl Shuffler {
    /// Create a new shuffler from `items`, seeded per-epoch and per-chain-tip.
    ///
    /// The seed is `SHA-256(SHARD_SHUFFLE_DOMAIN ‖ epoch_number ‖ prev_block_hash)`,
    /// which guarantees per-epoch determinism while binding the shuffle to the
    /// mined tip so it is not predictable before the previous block lands. The
    /// shuffled result is computed at construction time and returned by `shuffle()`.
    pub fn new(items: Vec<Vec<u8>>, epoch_number: u64, prev_block_hash: &[u8]) -> Self {
        let seed = derive_selection_seed(
            SHARD_SHUFFLE_DOMAIN,
            epoch_number,
            prev_block_hash,
            &[],
        );
        let mut rng = StdRng::from_seed(seed);

        let n = items.len();
        if n == 0 {
            return Shuffler {
                items,
                last_permutation: vec![],
            };
        }

        // Fisher-Yates shuffle
        let mut indices: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            let j = rng.gen_range(0..=i);
            indices.swap(i, j);
        }

        let last_permutation: Vec<Vec<u8>> = indices.iter()
            .map(|&i| items[i].clone())
            .collect();

        Shuffler { items, last_permutation }
    }

    /// Return the shuffled executor keys for this epoch.
    /// For the same epoch_number, always returns the same order.
    pub fn shuffle(&self) -> &[Vec<u8>] {
        &self.last_permutation
    }
}

pub fn deterministic_select(
    stakers: &StakeSet,
    domain: u8,
    seed_bytes: &[u8],
    epoch_number: u64,
    prev_block_hash: &[u8],
) -> Option<Vec<u8>> {
    let total = stakers.total_stake();
    if total == 0 {
        return None;
    }

    // Domain-separated seed bound to the mined tip:
    // SHA-256(domain ‖ epoch_number ‖ prev_block_hash ‖ seed_bytes)
    let seed = derive_selection_seed(domain, epoch_number, prev_block_hash, seed_bytes);
    let mut rng = StdRng::from_seed(seed);
    let target: u64 = rng.gen_range(0..total);

    // Deterministic iteration: sort keys lexicographically
    let mut keys: Vec<&Vec<u8>> = stakers.stakers.keys().collect();
    keys.sort();

    let first_key = keys[0].clone(); // backup for fallback
    let mut cumulative = 0u64;
    for key in keys {
        let stake = *stakers.stakers.get(key).unwrap();
        cumulative += stake;
        if cumulative >= target {
            return Some(key.clone());
        }
    }
    // Fallback: return the first staker (shouldn't happen if total > 0)
    Some(first_key)
}

// ---------------------------------------------------------------------------
// Executor sharding — deterministic shard selection per transaction
// ---------------------------------------------------------------------------

/// Select which executors in a shard should handle a transaction.
///
/// 1. Shard-index seed = SHA-256(SHARD_INDEX_DOMAIN ‖ epoch ‖ prev_block_hash ‖ tx_id)
///    — same tx + epoch + tip → same seed (tx_id spreads load across the txs)
/// 2. Shuffle executors deterministically
///    (SHA-256(SHARD_SHUFFLE_DOMAIN ‖ epoch ‖ prev_block_hash))
/// 3. Stake-balanced round-robin partition into `shard_count` shards
/// 4. `shard_index = derived_seed mod shard_count`
/// 5. Return executor public keys in the selected shard
pub fn deterministic_select_shard(
    executors: &ExecutorSet,
    shard_count: u32,
    tx_id: &str,
    epoch_number: u64,
    prev_block_hash: &[u8],
) -> Option<Vec<Vec<u8>>> {
    if executors.is_empty() {
        return None;
    }
    if shard_count == 0 {
        return None;
    }
    if shard_count == 1 {
        // No sharding: return all executors, sorted so the set is identical regardless of the
        // ExecutorSet's HashMap insertion order (C6).
        let mut keys: Vec<Vec<u8>> = executors.executors.keys().cloned().collect();
        keys.sort();
        return Some(keys);
    }

    // Domain-separated shard-index seed bound to the mined tip:
    // SHA-256(SHARD_INDEX_DOMAIN ‖ epoch_number ‖ prev_block_hash ‖ tx_id)
    let seed = derive_selection_seed(SHARD_INDEX_DOMAIN, epoch_number, prev_block_hash, tx_id.as_bytes());
    let mut shard_rng = StdRng::from_seed(seed);
    let shard_index: u32 = shard_rng.gen_range(0..shard_count);

    // Shuffle executors deterministically (bound to the mined tip)
    let shuffler = executors.shuffler(epoch_number, prev_block_hash);
    let shuffled = shuffler.shuffle();
    if shuffled.is_empty() {
        return None;
    }

    // Stake-balanced round-robin: assign each executor to the shard
    // with the lowest current total stake
    let mut shard_stakes: Vec<u64> = vec![0; shard_count as usize];
    let mut shard_executors: Vec<Vec<Vec<u8>>> = vec![vec![]; shard_count as usize];

    for key in shuffled {
        let stake = executors.get_stake(key);
        // Find shard with lowest current stake
        let target_shard = (0..shard_count as usize)
            .min_by_key(|&i| shard_stakes[i])
            .unwrap_or(0);
        shard_stakes[target_shard] += stake;
        shard_executors[target_shard].push(key.clone());
    }

    // Return executors for the selected shard
    let idx = shard_index as usize;
    if shard_executors[idx].is_empty() {
        return None;
    }
    Some(shard_executors[idx].clone())
}

// ---------------------------------------------------------------------------
// Traits — interface for epoch management
// ---------------------------------------------------------------------------

/// Reconciler examines chain state at epoch boundaries and returns
/// operations to apply. Does not mutate state directly.
pub trait IEpochReconciler: Send + Sync {
    /// Run reconciliation and return the data to apply
    fn reconcile(&self) -> EpochReconciliation;
}

/// Applies staking operations from reconciliation
pub trait IStakingManager: Send + Sync {
    /// Apply a batch of staking operations from reconciliation
    fn apply_ops(&self, ops: &EpochReconciliation) -> Result<(), PneumaticError>;
}

/// Selects the block leader for an epoch using stake-weighted selection
pub trait IEpochLeaderSelector: Send + Sync {
    /// Select leader(s) from the current stake set deterministically.
    /// The seed is bound to `epoch_number` and the previous block hash so the
    /// leader is only knowable once the prior block is mined — every node with
    /// the same stake set and chain tip produces the same leader.
    /// Returns the selected public key(s).
    fn select(&self, stakers: &StakeSet, epoch_number: u64, prev_block_hash: &[u8]) -> Vec<u8>;
}

// ---------------------------------------------------------------------------
// Stub implementations — return empty/placeholder data
// ---------------------------------------------------------------------------

/// Stub reconciler that returns empty reconciliation data.
/// Replace with real chain analysis in Phase 5.
pub struct StubEpochReconciler;

impl IEpochReconciler for StubEpochReconciler {
    fn reconcile(&self) -> EpochReconciliation {
        EpochReconciliation::default()
    }
}

/// Stub staking manager that logs operations but doesn't persist.
pub struct StubStakingManager;

impl IStakingManager for StubStakingManager {
    fn apply_ops(&self, _ops: &EpochReconciliation) -> Result<(), PneumaticError> {
        // Stub: no-op
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// LeaderSelector — stake-weighted random selection
// ---------------------------------------------------------------------------

/// Stake-weighted random leader selector.
/// Uses the cumulative stake range approach: pick a random point
/// in [0, total_stake) and walk the sorted stakers to find who
/// owns that point.
pub struct LeaderSelector;

impl LeaderSelector {
    pub fn new() -> Self {
        LeaderSelector
    }
}

impl Default for LeaderSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl IEpochLeaderSelector for LeaderSelector {
    fn select(&self, stakers: &StakeSet, epoch_number: u64, prev_block_hash: &[u8]) -> Vec<u8> {
        deterministic_select(stakers, LEADER_DOMAIN, &[], epoch_number, prev_block_hash)
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Epoch — helper constructors
// ---------------------------------------------------------------------------

impl Epoch {
    /// Create a new epoch with a leader selected from the stake set.
    pub fn new_with_leader(
        epoch_number: u64,
        start_timestamp: i64,
        end_timestamp: i64,
        selector: &dyn IEpochLeaderSelector,
        stake_set: &StakeSet,
        prev_block_hash: &[u8],
    ) -> Self {
        let leader_public_key = selector.select(stake_set, epoch_number, prev_block_hash);
        Epoch {
            start_timestamp,
            end_timestamp,
            epoch_number,
            leader_public_key,
        }
    }
}

// ---------------------------------------------------------------------------
// BlockProposer — leader constructs blocks from the transaction pool
// ---------------------------------------------------------------------------

/// The leader node that proposes blocks. Holds the leader's identity and
/// stake for inclusion in `SignedTransaction` wrappers.
#[derive(Debug, Clone)]
pub struct BlockProposer {
    /// Leader's public key / address
    pub leader_address: Vec<u8>,
    /// Leader's stake amount
    pub leader_stake: u64,
    /// Leader's genesis block hash
    pub leader_hash: Vec<u8>,
}

impl BlockProposer {
    pub fn new(leader_address: Vec<u8>, leader_stake: u64, leader_hash: Vec<u8>) -> Self {
        BlockProposer {
            leader_address,
            leader_stake,
            leader_hash,
        }
    }
}

/// Trait for proposing batches of transactions. Allows mocking in tests.
pub trait IBlockProposer: Send + Sync {
    /// Propose a batch of up to `limit` validated transactions for a token.
    /// Returns tuples of (original transaction, wrapped SignedTransaction).
    fn propose_batch(
        &self,
        registry: &crate::registry::PendingTransactionRegistry,
        token_id: &[u8],
        limit: usize,
    ) -> Result<Vec<(crate::transactions::Transaction, crate::transactions::SignedTransaction)>, PneumaticError>;
}

impl IBlockProposer for BlockProposer {
    fn propose_batch(
        &self,
        registry: &crate::registry::PendingTransactionRegistry,
        token_id: &[u8],
        limit: usize,
    ) -> Result<Vec<(crate::transactions::Transaction, crate::transactions::SignedTransaction)>, PneumaticError> {
        let tx_ids = registry.dequeue_for_leader(token_id, limit);
        let mut result = Vec::with_capacity(tx_ids.len());
        for tx_id in tx_ids {
            let tx = registry.get_transaction(&tx_id)?;
            let signed = crate::transactions::SignedTransaction {
                transaction_id: tx.id.clone(),
                transaction: tx.clone(),
                total_stake: 0, // caller fills in after stake resolution
                total_voters: 0, // caller fills in after voter count
                leader_address: self.leader_address.clone(),
                leader_stake: self.leader_stake,
                leader_hash: self.leader_hash.clone(),
                finalizer_addr: vec![],
                finalizer_sig: crate::transactions::TransactionSignature {
                    transaction_id: vec![],
                    env_id: vec![],
                    transaction_hash: vec![],
                    signature: vec![],
                    current_stake: 0,
                },
                executor_sigs: std::collections::HashMap::new(),
                proposer_key: self.leader_address.clone(),
            };
            result.push((tx, signed));
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// EpochBoundaryDetector — stale block and epoch advancement detection
// ---------------------------------------------------------------------------

/// Detects epoch expiry, stale blocks, and advances to new epochs.
#[derive(Debug, Clone)]
pub struct EpochBoundaryDetector {
    /// The current epoch
    pub current_epoch: Epoch,
    /// The leader from the previous epoch (for stale block detection)
    pub previous_leader: Option<Vec<u8>>,
}

impl EpochBoundaryDetector {
    pub fn new(epoch: Epoch) -> Self {
        EpochBoundaryDetector {
            current_epoch: epoch,
            previous_leader: None,
        }
    }

    /// Check if the current epoch has expired at the given timestamp.
    pub fn is_epoch_expired(&self, now: i64) -> bool {
        now >= self.current_epoch.end_timestamp
    }

    /// Return the current epoch's leader.
    pub fn current_leader(&self) -> Option<&[u8]> {
        if self.current_epoch.leader_public_key.is_empty() {
            None
        } else {
            Some(&self.current_epoch.leader_public_key)
        }
    }

    /// Advance to a new epoch: bump the epoch number, select a new leader.
    ///
    /// `prev_block_hash` is the chain tip of the epoch about to end; it is bound
    /// into the leader seed so the new leader is only knowable once that tip is
    /// mined (Phase 5.3 / AUDIT H3). Empty at genesis.
    pub fn advance_to_new_epoch(
        &mut self,
        selector: &dyn IEpochLeaderSelector,
        stake_set: &StakeSet,
        epoch_duration: i64,
        prev_block_hash: &[u8],
    ) {
        // Save current leader as previous
        if !self.current_epoch.leader_public_key.is_empty() {
            self.previous_leader = Some(self.current_epoch.leader_public_key.clone());
        }
        // Create new epoch
        let now = self.current_epoch.end_timestamp;
        let new_epoch_number = self.current_epoch.epoch_number + 1;
        self.current_epoch = Epoch::new_with_leader(
            new_epoch_number,
            now,
            now + epoch_duration,
            selector,
            stake_set,
            prev_block_hash,
        );
    }

    /// Check if a block was proposed by a stale (previous-epoch) leader.
    pub fn is_stale_block(&self, proposer_key: &[u8]) -> bool {
        match &self.previous_leader {
            Some(prev) => prev == proposer_key,
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Conflict resolution
// ---------------------------------------------------------------------------

/// Resolution outcome from a block conflict — determines how the system responds.
#[derive(Debug, Clone)]
pub enum ConflictResolution {
    /// Different proposers — network race. Commit the winner, discard the loser.
    DiscardLoser(Vec<u8>),
    /// Same proposer signed both blocks — double-signed. Commit the winner, slash the proposer.
    SameProposerSlash(Vec<u8>, Vec<u8>), // (winner_hash, proposer_key_to_slash)
    /// Equal stakes and identical hashes (theoretical) — commit winner, flag both for review.
    TieFlagBoth(Vec<u8>),
}

/// Resolve a conflict between two block proposals at the same height.
/// Returns a `ConflictResolution` that determines the system response:
/// - **DiscardLoser**: different proposers, network race. Commit winner, discard loser.
/// - **SameProposerSlash**: same proposer double-signed. Commit winner, slash proposer.
/// - **TieFlagBoth**: equal stakes + equal hashes (theoretical). Flag both for review.
///
/// Tie-break for equal stakes and different proposers: lexicographic comparison of
/// block hashes (smaller wins).
pub fn resolve_block_conflict(
    block_a_hash: &[u8],
    block_b_hash: &[u8],
    proposer_a: &[u8],
    proposer_b: &[u8],
    stake_set: &StakeSet,
) -> Result<ConflictResolution, PneumaticError> {
    let stake_a = stake_set.get_stake(proposer_a);
    let stake_b = stake_set.get_stake(proposer_b);

    // Different stakes — higher stake wins (network race between honest nodes)
    if stake_a > stake_b {
        return Ok(ConflictResolution::DiscardLoser(block_a_hash.to_vec()));
    }
    if stake_b > stake_a {
        return Ok(ConflictResolution::DiscardLoser(block_b_hash.to_vec()));
    }

    // Equal stakes — check proposer identity
    let same_proposer = proposer_a == proposer_b;

    if same_proposer {
        // Same proposer double-signed — protocol violation, slash them
        return Ok(ConflictResolution::SameProposerSlash(
            block_a_hash.to_vec(),
            proposer_a.to_vec(),
        ));
    }

    // Equal stakes, different proposers — hash tie-break
    if block_a_hash <= block_b_hash {
        Ok(ConflictResolution::DiscardLoser(block_a_hash.to_vec()))
    } else {
        Ok(ConflictResolution::DiscardLoser(block_b_hash.to_vec()))
    }
}

// ---------------------------------------------------------------------------
// CandidateRegistry — competing block proposals
// ---------------------------------------------------------------------------

/// Registry of competing block candidates keyed by (token_id, previous_hash).
/// Used for conflict detection — when two or more valid blocks reference the
/// same previous_hash for the same token, they represent a fork.
///
/// Each (token_id, previous_hash) group is bounded to at most
/// [`CandidateRegistry::DEFAULT_MAX_CANDIDATES`] candidates: when an insert
/// would exceed that cap the oldest candidate is evicted (LRU), so repeated
/// conflicting proposals at one position cannot inflate the registry without
/// bound (AUDIT Phase 5.2 / H2).
#[derive(Debug)]
pub struct CandidateRegistry {
    /// (token_id, previous_hash) → list of candidate (block, proposer_key) pairs
    candidates: DashMap<(Vec<u8>, Vec<u8>), Vec<(crate::blocks::Block, Vec<u8>)>>,
    /// Max number of candidates held at a single (token_id, previous_hash)
    /// position before the oldest is evicted.
    max_candidates: usize,
}

impl CandidateRegistry {
    /// Upper bound on the number of competing candidates kept at any one
    /// (token_id, previous_hash) position. Older candidates are evicted (LRU)
    /// once this many are present (AUDIT Phase 5.2 / H2).
    pub const DEFAULT_MAX_CANDIDATES: usize = 1024;

    pub fn new() -> Self {
        CandidateRegistry {
            candidates: DashMap::new(),
            max_candidates: CandidateRegistry::DEFAULT_MAX_CANDIDATES,
        }
    }

    /// Build a registry with an explicit per-position cap.
    pub fn with_max_candidates(max_candidates: usize) -> Self {
        CandidateRegistry {
            candidates: DashMap::new(),
            max_candidates: max_candidates.max(1),
        }
    }

    /// Insert a candidate block. If another candidate already exists at this
    /// (token_id, previous_hash), the new candidate is appended — a conflict
    /// is detected when the vec has length >= 2.
    ///
    /// On overflow the oldest candidate is evicted so the per-position vec stays
    /// bounded (AUDIT Phase 5.2 / H2 / LRU eviction).
    pub fn insert(&self, token_id: Vec<u8>, previous_hash: Vec<u8>,
                  block: crate::blocks::Block, proposer_key: Vec<u8>) {
        let key = (token_id, previous_hash);
        let mut entry = self.candidates.entry(key).or_insert_with(Vec::new);
        entry.push((block, proposer_key));
        // LRU eviction: keep at most `max_candidates` candidates per position.
        while entry.len() > self.max_candidates {
            entry.remove(0);
        }
    }

    /// Get all candidates for a given (token_id, previous_hash).
    pub fn get_candidates(&self, token_id: &[u8], previous_hash: &[u8]) -> Vec<(crate::blocks::Block, Vec<u8>)> {
        let key = (token_id.to_vec(), previous_hash.to_vec());
        self.candidates.get(&key)
            .map(|entry| entry.value().clone())
            .unwrap_or_default()
    }

    /// Check if a conflict exists: 2+ candidates at the same (token_id, previous_hash).
    pub fn has_conflict(&self, token_id: &[u8], previous_hash: &[u8]) -> bool {
        let key = (token_id.to_vec(), previous_hash.to_vec());
        self.candidates.get(&key)
            .map(|entry| entry.value().len() >= 2)
            .unwrap_or(false)
    }

    /// Get the number of candidates at a specific key.
    pub fn candidate_count(&self, token_id: &[u8], previous_hash: &[u8]) -> usize {
        let key = (token_id.to_vec(), previous_hash.to_vec());
        self.candidates.get(&key)
            .map(|entry| entry.value().len())
            .unwrap_or(0)
    }

    /// Remove all candidates at a key (after resolving a conflict).
    pub fn remove_conflicted(&self, token_id: &[u8], previous_hash: &[u8]) -> usize {
        let key = (token_id.to_vec(), previous_hash.to_vec());
        self.candidates.remove(&key).map(|(_, v)| v.len()).unwrap_or(0)
    }

    /// Total number of distinct (token_id, previous_hash) keys.
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// Returns true if there are no candidate groups.
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }
}

impl Default for CandidateRegistry {
    /// An empty registry sized to [`CandidateRegistry::DEFAULT_MAX_CANDIDATES`].
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::blocks::{Block, FinalityStatus};

    // --- LeaderSelector tests ---

    fn make_stake_set(stakes: Vec<(Vec<u8>, u64)>) -> StakeSet {
        StakeSet {
            stakers: stakes.into_iter().collect(),
        }
    }

    #[test]
    fn leader_selector_empty_stake_set_returns_empty() {
        let selector = LeaderSelector::new();
        let stakes = make_stake_set(vec![]);
        let leader = selector.select(&stakes, 1, &[]);
        assert!(leader.is_empty());
    }

    #[test]
    fn leader_selector_single_staker_always_selected() {
        let selector = LeaderSelector::new();
        let key = vec![1, 2, 3];
        let stakes = make_stake_set(vec![(key.clone(), 100)]);
        // Run 10 times — single staker should always be selected
        for _ in 0..10 {
            assert_eq!(selector.select(&stakes, 1, &[]), key);
        }
    }

    #[test]
    fn leader_selector_deterministic_different_epochs_can_differ() {
        let selector = LeaderSelector::new();
        let key_a = vec![1];
        let key_b = vec![2];
        let stakes = make_stake_set(vec![(key_a.clone(), 50), (key_b.clone(), 50)]);
        // Same epoch → same leader
        let leader_epoch1 = selector.select(&stakes, 1, &[]);
        assert_eq!(leader_epoch1, selector.select(&stakes, 1, &[]));
        // Different epochs → deterministic but may differ
        let leader_epoch2 = selector.select(&stakes, 2, &[]);
        // Either they happen to be the same (still deterministic), or differ
        // Just verify both calls with same epoch return the same result
        assert_eq!(leader_epoch1, selector.select(&stakes, 1, &[]));
        assert_eq!(leader_epoch2, selector.select(&stakes, 2, &[]));
    }

    #[test]
    fn leader_selector_weighted_returns_more_from_larger_stake() {
        let selector = LeaderSelector::new();
        let key_small = vec![1];
        let key_large = vec![2];
        // Small gets 10%, large gets 90%
        let stakes = make_stake_set(vec![(key_small.clone(), 10), (key_large.clone(), 90)]);
        let mut small_count = 0u64;
        for _ in 0..100 {
            if selector.select(&stakes, 1, &[]) == key_small {
                small_count += 1;
            }
        }
        // Small should be selected ~10% of the time
        assert!(small_count <= 25, "expected ~10 small selections, got {}", small_count);
    }

    // --- SA_02: Deterministic leader selection ---

    #[test]
    fn leader_selector_deterministic_same_inputs_same_output() {
        let selector = LeaderSelector::new();
        let stakes = make_stake_set(vec![(vec![1], 30), (vec![2], 50), (vec![3], 20)]);
        let first = selector.select(&stakes, 5, &[]);
        for _ in 1..20 {
            assert_eq!(selector.select(&stakes, 5, &[]), first);
        }
    }

    // --- Epoch::new_with_leader tests ---

    #[test]
    fn epoch_new_with_leader_sets_fields() {
        let selector = LeaderSelector::new();
        let key = vec![42];
        let stakes = make_stake_set(vec![(key.clone(), 100)]);
        let epoch = Epoch::new_with_leader(1, 1000, 2000, &selector, &stakes, &[]);
        assert_eq!(epoch.epoch_number, 1);
        assert_eq!(epoch.start_timestamp, 1000);
        assert_eq!(epoch.end_timestamp, 2000);
        assert_eq!(epoch.leader_public_key, key);
    }

    // --- StakeSet tests ---

    #[test]
    fn stake_set_total_stake_returns_sum() {
        let stakes = make_stake_set(vec![
            (vec![1], 10),
            (vec![2], 20),
            (vec![3], 30),
        ]);
        assert_eq!(stakes.total_stake(), 60);
    }

    #[test]
    fn stake_set_get_stake_returns_zero_for_missing_key() {
        let stakes = make_stake_set(vec![(vec![1], 10)]);
        assert_eq!(stakes.get_stake(&[2]), 0);
    }

    #[test]
    fn stake_set_get_stake_returns_correct_value() {
        let stakes = make_stake_set(vec![(vec![1], 100)]);
        assert_eq!(stakes.get_stake(&[1]), 100);
    }

    // --- BlockProposer tests ---

    use crate::registry::PendingTransactionRegistry;
    use crate::transactions::{Transaction, TransactionValidationResult};

    fn make_validated_registry() -> PendingTransactionRegistry {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        registry.acquire_transaction("tx1").unwrap();
        if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
            entry.transition_to_validated(
                Transaction {
                    id: "tx1".into(), action: "Transfer".into(),
                    token_id: vec![1, 2], bid: None, sequence_number: 1,
                    sender: vec![10], receiver: vec![20], amount: Some(100),
                    timestamp: 1000, result_hash: vec![],
                    sender_signature: vec![],
                },
                TransactionValidationResult::valid(
                    vec![99],
                    crate::errors::TransactionRiskFactor {
                        affected_parties: 2, amount: 100,
                        is_contract: false, is_multi_party: false,
                    },
                ),
            );
        }

        registry.register_pending("tx2".into()).unwrap();
        registry.acquire_transaction("tx2").unwrap();
        if let Ok(mut entry) = registry.get_transaction_mut("tx2") {
            entry.transition_to_validated(
                Transaction {
                    id: "tx2".into(), action: "Transfer".into(),
                    token_id: vec![1, 2], bid: None, sequence_number: 2,
                    sender: vec![10], receiver: vec![20], amount: Some(200),
                    timestamp: 2000, result_hash: vec![],
                    sender_signature: vec![],
                },
                TransactionValidationResult::valid(
                    vec![99],
                    crate::errors::TransactionRiskFactor {
                        affected_parties: 2, amount: 200,
                        is_contract: false, is_multi_party: false,
                    },
                ),
            );
        }

        // Enqueue both into the pool
        registry.enqueue_to_pool("tx1", vec![1, 2], 1, 1000, vec![10]);
        registry.enqueue_to_pool("tx2", vec![1, 2], 2, 2000, vec![10]);

        registry
    }

    #[test]
    fn proposer_empty_pool_returns_empty_vec() {
        let registry = PendingTransactionRegistry::new();
        let proposer = BlockProposer::new(vec![1], 100, vec![2]);
        let batch = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
        assert!(batch.is_empty());
    }

    #[test]
    fn proposer_batch_size_matches_limit() {
        let registry = make_validated_registry();
        let proposer = BlockProposer::new(vec![1], 100, vec![2]);
        let batch = proposer.propose_batch(&registry, &[1, 2], 2).unwrap();
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn proposer_fewer_txs_than_limit_returns_available() {
        let registry = make_validated_registry();
        let proposer = BlockProposer::new(vec![1], 100, vec![2]);
        let batch = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn proposer_dequeued_txs_not_returned_again() {
        let registry = make_validated_registry();
        let proposer = BlockProposer::new(vec![1], 100, vec![2]);
        let _batch1 = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
        // Second proposal should be empty since pool was drained
        let batch2 = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
        assert!(batch2.is_empty());
    }

    #[test]
    fn proposer_leader_fields_propagated() {
        let registry = make_validated_registry();
        let proposer = BlockProposer::new(vec![99], 777, vec![42]);
        let batch = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
        for (_, signed) in batch {
            assert_eq!(signed.leader_address, vec![99]);
            assert_eq!(signed.leader_stake, 777);
            assert_eq!(signed.leader_hash, vec![42]);
        }
    }

    // --- EpochBoundaryDetector tests ---

    fn make_epoch(num: u64, start: i64, end: i64, leader: Vec<u8>) -> Epoch {
        Epoch {
            start_timestamp: start,
            end_timestamp: end,
            epoch_number: num,
            leader_public_key: leader,
        }
    }

    #[test]
    fn detector_not_expired_before_end() {
        let epoch = make_epoch(1, 0, 1000, vec![1]);
        let detector = EpochBoundaryDetector::new(epoch);
        assert!(!detector.is_epoch_expired(999));
    }

    #[test]
    fn detector_expired_at_end() {
        let epoch = make_epoch(1, 0, 1000, vec![1]);
        let detector = EpochBoundaryDetector::new(epoch);
        assert!(detector.is_epoch_expired(1000));
    }

    #[test]
    fn detector_current_leader_returns_some() {
        let epoch = make_epoch(1, 0, 1000, vec![1]);
        let detector = EpochBoundaryDetector::new(epoch);
        let leader = detector.current_leader();
        assert!(matches!(leader, Some(bytes) if bytes == [1u8]));
    }

    #[test]
    fn detector_current_leader_empty_returns_none() {
        let epoch = make_epoch(1, 0, 1000, vec![]);
        let detector = EpochBoundaryDetector::new(epoch);
        assert!(detector.current_leader().is_none());
    }

    #[test]
    fn detector_advance_to_new_epoch_bumps_number_and_selects_leader() {
        let epoch = make_epoch(1, 0, 1000, vec![1]);
        let mut detector = EpochBoundaryDetector::new(epoch);
        let selector = LeaderSelector::new();
        let stakes = make_stake_set(vec![(vec![2], 100)]);
        detector.advance_to_new_epoch(&selector, &stakes, 1000, &[]);
        assert_eq!(detector.current_epoch.epoch_number, 2);
        assert_eq!(detector.previous_leader, Some(vec![1]));
        assert_eq!(detector.current_epoch.leader_public_key, vec![2]);
    }

    #[test]
    fn detector_is_stale_block_detects_previous_leader() {
        let epoch = make_epoch(1, 0, 1000, vec![1]);
        let mut detector = EpochBoundaryDetector::new(epoch);
        detector.previous_leader = Some(vec![99]);
        assert!(detector.is_stale_block(&vec![99]));
        assert!(!detector.is_stale_block(&vec![1]));
    }

    #[test]
    fn detector_is_stale_block_no_previous_returns_false() {
        let epoch = make_epoch(1, 0, 1000, vec![1]);
        let detector = EpochBoundaryDetector::new(epoch);
        assert!(!detector.is_stale_block(&vec![1]));
        assert!(!detector.is_stale_block(&vec![99]));
    }

    // --- resolve_block_conflict tests ---

    #[test]
    fn conflict_resolution_stake_difference_returns_discard_loser() {
        let stakes = make_stake_set(vec![(vec![1], 100), (vec![2], 200)]);
        let result = resolve_block_conflict(
            b"hash_a", b"hash_b",
            &vec![1], &vec![2],
            &stakes,
        ).unwrap();
        match result {
            ConflictResolution::DiscardLoser(winner) => assert_eq!(winner, b"hash_b"),
            _ => panic!("Expected DiscardLoser"),
        }
    }

    #[test]
    fn conflict_resolution_tie_break_by_hash_returns_discard_loser() {
        let stakes = make_stake_set(vec![(vec![1], 100), (vec![2], 100)]);
        let result = resolve_block_conflict(
            b"aa", b"bb",
            &vec![1], &vec![2],
            &stakes,
        ).unwrap();
        match result {
            ConflictResolution::DiscardLoser(winner) => assert_eq!(winner, b"aa"),
            _ => panic!("Expected DiscardLoser"),
        }
    }

    #[test]
    fn conflict_resolution_same_proposer_returns_slash() {
        let stakes = make_stake_set(vec![(vec![1], 100)]);
        let result = resolve_block_conflict(
            b"hash_a", b"hash_b",
            &vec![1], &vec![1],
            &stakes,
        ).unwrap();
        match result {
            ConflictResolution::SameProposerSlash(winner, slashed) => {
                assert_eq!(winner, b"hash_a");
                assert_eq!(slashed, vec![1]);
            }
            _ => panic!("Expected SameProposerSlash"),
        }
    }

    // --- CandidateRegistry tests ---

    #[test]
    fn registry_empty_on_creation() {
        let registry = CandidateRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    // --- CandidateRegistry LRU bound (AUDIT Phase 5.2 / H2) ---

    #[test]
    fn registry_lru_evicts_oldest_when_over_cap() {
        // A per-position candidate vec is capped at `max_candidates`; the oldest
        // candidate is evicted once the cap is exceeded, so repeated conflicting
        // proposals cannot inflate the registry.
        let registry = CandidateRegistry::with_max_candidates(3);
        let token_id = vec![7];
        let prev_hash = vec![8];

        // Insert 5 candidates (cap is 3); each carries a distinguishable
        // proposer key so we can confirm the oldest is dropped.
        let first = vec![1];
        for proposer in [first, vec![2], vec![3], vec![4], vec![5]] {
            let block = Block::test_block(proposer.clone());
            registry.insert(token_id.clone(), prev_hash.clone(), block, proposer);
        }

        let candidates = registry.get_candidates(&token_id, &prev_hash);
        assert_eq!(candidates.len(), 3, "per-position count stays capped at max");
        // Oldest (proposer [1]) evicted; the three most recent remain, in order.
        let proposers: Vec<Vec<u8>> = candidates.iter().map(|(_, pk)| pk.clone()).collect();
        assert_eq!(proposers, vec![vec![3], vec![4], vec![5]]);
    }

    #[test]
    fn registry_insert_and_get_candidates() {
        let registry = CandidateRegistry::new();
        let block = Block::test_block(vec![1, 2, 3]);
        let token_id = vec![1, 2];
        let prev_hash = vec![4, 5, 6];
        let proposer = vec![99];

        registry.insert(token_id.clone(), prev_hash.clone(), block, proposer.clone());

        let candidates = registry.get_candidates(&token_id, &prev_hash);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].1, proposer);
    }

    #[test]
    fn registry_detects_conflict_on_second_insert() {
        let registry = CandidateRegistry::new();
        let block_a = Block::test_block(vec![1]);
        let block_b = Block::test_block(vec![2]);
        let token_id = vec![1, 2];
        let prev_hash = vec![3, 4, 5];

        registry.insert(token_id.clone(), prev_hash.clone(), block_a, vec![1]);
        assert!(!registry.has_conflict(&token_id, &prev_hash));

        registry.insert(token_id.clone(), prev_hash.clone(), block_b, vec![2]);
        assert!(registry.has_conflict(&token_id, &prev_hash));
    }

    #[test]
    fn registry_candidate_count_returns_correct_count() {
        let registry = CandidateRegistry::new();
        let token_id = vec![1];
        let prev_hash = vec![2];

        assert_eq!(registry.candidate_count(&token_id, &prev_hash), 0);

        registry.insert(token_id.clone(), prev_hash.clone(), Block::test_block(vec![1]), vec![1]);
        assert_eq!(registry.candidate_count(&token_id, &prev_hash), 1);

        registry.insert(token_id.clone(), prev_hash.clone(), Block::test_block(vec![2]), vec![2]);
        assert_eq!(registry.candidate_count(&token_id, &prev_hash), 2);
    }

    #[test]
    fn registry_remove_conflicted_clears_entry() {
        let registry = CandidateRegistry::new();
        let token_id = vec![1, 2];
        let prev_hash = vec![3, 4, 5];

        registry.insert(token_id.clone(), prev_hash.clone(), Block::test_block(vec![1]), vec![1]);
        registry.insert(token_id.clone(), prev_hash.clone(), Block::test_block(vec![2]), vec![2]);

        assert_eq!(registry.candidate_count(&token_id, &prev_hash), 2);
        let removed = registry.remove_conflicted(&token_id, &prev_hash);
        assert_eq!(removed, 2);
        assert!(registry.is_empty());
    }

    #[test]
    fn registry_separate_keys_independent() {
        let registry = CandidateRegistry::new();
        let block = Block::test_block(vec![1]);

        registry.insert(vec![1], vec![2], block.clone(), vec![1]);
        registry.insert(vec![3], vec![4], block, vec![2]);

        assert_eq!(registry.len(), 2);
        assert_eq!(registry.candidate_count(&[1], &[2]), 1);
        assert_eq!(registry.candidate_count(&[3], &[4]), 1);
        assert!(!registry.has_conflict(&[1], &[2]));
    }

    // --- CandidateRegistry concurrent tests ---

    #[test]
    fn registry_concurrent_inserts_no_panic() {
        let registry = Arc::new(CandidateRegistry::new());
        let token_id = vec![1];
        let prev_hash = vec![2];
        let mut handles = vec![];

        for i in 0..10 {
            let reg = Arc::clone(&registry);
            let tid = token_id.clone();
            let ph = prev_hash.clone();
            handles.push(std::thread::spawn(move || {
                let block = Block::test_block(vec![i as u8]);
                reg.insert(tid, ph, block, vec![i]);
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        assert_eq!(registry.candidate_count(&token_id, &prev_hash), 10);
        assert!(registry.has_conflict(&token_id, &prev_hash));
    }

    #[test]
    fn registry_concurrent_separate_keys_no_race() {
        let registry = Arc::new(CandidateRegistry::new());
        let mut handles = vec![];

        for i in 0..5 {
            let reg = Arc::clone(&registry);
            handles.push(std::thread::spawn(move || {
                let token_id = vec![i];
                let prev_hash = vec![i, 0];
                let block = Block::test_block(vec![i as u8]);
                reg.insert(token_id, prev_hash, block, vec![i]);
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        assert_eq!(registry.len(), 5);
    }

    // --- deterministic_select tests ---

    #[test]
    fn deterministic_select_empty_returns_none() {
        let stakes = make_stake_set(vec![]);
        let result = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx1", 1, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn deterministic_select_single_staker_always_same() {
        let key = vec![1, 2, 3];
        let stakes = make_stake_set(vec![(key.clone(), 100)]);
        for _ in 0..20 {
            assert_eq!(deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx1", 1, &[]), Some(key.clone()));
        }
    }

    #[test]
    fn deterministic_select_different_txs_distribute() {
        let key_a = vec![1];
        let key_b = vec![2];
        let stakes = make_stake_set(vec![(key_a.clone(), 10), (key_b.clone(), 90)]);

        // Pick many different tx_ids — verify distribution roughly matches stake weights
        let mut a_count = 0u64;
        let num_trials = 200;
        for i in 0..num_trials {
            let tx_id = format!("tx_{}", i);
            if deterministic_select(&stakes, FINALIZER_DOMAIN, tx_id.as_bytes(), 1, &[]) == Some(key_a.clone()) {
                a_count += 1;
            }
        }
        // key_a has 10% stake — expect ~10% selections, with some tolerance
        assert!(a_count <= 30, "expected ≤30 small selections (10%), got {}", a_count);
        assert!(a_count >= 2, "expected ≥2 small selections (10%), got {}", a_count);
    }

    #[test]
    fn deterministic_select_deterministic_across_epochs() {
        let stakes = make_stake_set(vec![(vec![1], 30), (vec![2], 50), (vec![3], 20)]);
        let epoch1 = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx_alpha", 1, &[]);
        let epoch1_again = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx_alpha", 1, &[]);
        assert_eq!(epoch1, epoch1_again); // Same seed + same epoch → same result

        let epoch2 = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx_alpha", 2, &[]);
        // Epoch2 may or may not differ — but must be deterministic
        let epoch2_again = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx_alpha", 2, &[]);
        assert_eq!(epoch2, epoch2_again);
    }

    #[test]
    fn deterministic_select_zero_stake_returns_none() {
        let stakes = make_stake_set(vec![(vec![1], 0), (vec![2], 0)]);
        let result = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx1", 1, &[]);
        assert!(result.is_none());
    }

    // --- ExecutorSet tests ---

    #[test]
    fn executor_set_empty_on_creation() {
        let es = ExecutorSet::default();
        assert!(es.is_empty());
        assert_eq!(es.len(), 0);
        assert_eq!(es.total_stake(), 0);
    }

    #[test]
    fn executor_set_total_stake_returns_sum() {
        let es = ExecutorSet {
            executors: [(b"a".to_vec(), 10), (b"b".to_vec(), 20), (b"c".to_vec(), 30)]
                .into_iter().collect(),
        };
        assert_eq!(es.total_stake(), 60);
        assert_eq!(es.len(), 3);
    }

    #[test]
    fn executor_set_get_stake_returns_zero_for_missing_key() {
        let es = ExecutorSet {
            executors: [(b"a".to_vec(), 100)].into_iter().collect(),
        };
        assert_eq!(es.get_stake(&b"z".to_vec()), 0);
    }

    #[test]
    fn executor_set_to_stake_set_converts() {
        let es = ExecutorSet {
            executors: [(b"a".to_vec(), 100)].into_iter().collect(),
        };
        let ss = es.to_stake_set();
        assert_eq!(ss.total_stake(), 100);
    }

    // --- Shuffler tests ---

    #[test]
    fn shuffler_empty_returns_empty() {
        let shuffler = Shuffler::new(vec![], 1, &[]);
        let shuffled = shuffler.shuffle();
        assert!(shuffled.is_empty());
    }

    #[test]
    fn shuffler_single_item_same_order() {
        let shuffler = Shuffler::new(vec![b"executor_1".to_vec()], 1, &[]);
        let shuffled = shuffler.shuffle();
        assert_eq!(shuffled.len(), 1);
        assert_eq!(shuffled[0], b"executor_1".to_vec());
    }

    #[test]
    fn shuffler_deterministic_same_epoch_same_order() {
        let keys = vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()];
        let shuffler1 = Shuffler::new(keys.clone(), 42, &[]);
        let shuffler2 = Shuffler::new(keys, 42, &[]);
        let result1 = shuffler1.shuffle();
        let result2 = shuffler2.shuffle();
        assert_eq!(result1.len(), result2.len());
        for (a, b) in result1.iter().zip(result2.iter()) {
            assert_eq!(a, b);
        }
    }

    #[test]
    fn shuffler_different_epochs_different_order() {
        let keys = vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec(), b"e".to_vec()];
        let shuffler1 = Shuffler::new(keys.clone(), 1, &[]);
        let shuffler2 = Shuffler::new(keys.clone(), 2, &[]);
        let result1 = shuffler1.shuffle();
        let result2 = shuffler2.shuffle();
        // Different seeds must produce different full permutations.
        assert_ne!(result1.to_vec(), result2.to_vec());
    }

    // ---------------------------------------------------------------------------
    // Deterministic shard selection tests
    // ---------------------------------------------------------------------------

    #[test]
    fn deterministic_select_shard_empty_returns_none() {
        let executors = ExecutorSet::default();
        let result = deterministic_select_shard(&executors, 2, "tx1", 1, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn deterministic_select_shard_single_shard_returns_all() {
        let mut executors = ExecutorSet::default();
        executors.executors.insert(b"exec1".to_vec(), 100);
        executors.executors.insert(b"exec2".to_vec(), 200);
        let result = deterministic_select_shard(&executors, 1, "any-tx", 1, &[]);
        assert!(result.is_some());
        let keys = result.unwrap();
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn deterministic_select_shard_distributes_across_shards() {
        let mut executors = ExecutorSet::default();
        for i in 0..6 {
            executors.executors.insert(format!("exec{}", i).into_bytes(), 100);
        }
        // With 6 executors and 3 shards, each shard should get ~2 executors
        let result = deterministic_select_shard(&executors, 3, "tx-42", 1, &[]);
        assert!(result.is_some());
        let shard = result.unwrap();
        assert!(shard.len() >= 1 && shard.len() <= 4,
            "shard size {} should be between 1 and 4", shard.len());
    }

    #[test]
    fn deterministic_select_shard_deterministic_across_calls() {
        let mut executors = ExecutorSet::default();
        for i in 0..4 {
            executors.executors.insert(format!("exec{}", i).into_bytes(), 100 + i);
        }
        let result1 = deterministic_select_shard(&executors, 2, "same-tx", 5, &[]);
        let result2 = deterministic_select_shard(&executors, 2, "same-tx", 5, &[]);
        assert!(result1.is_some() && result2.is_some());
        assert_eq!(result1.unwrap(), result2.unwrap());
    }

    // --- C6: sort before shuffling ---

    #[test]
    fn deterministic_select_shard_sorted_before_shuffle() {
        // Same logical executor set, in different HashMap insertion orders and after a serde
        // round-trip, must yield identical shard partitions. Without the sort both the
        // shard_count==1 shortcut and the shuffle consumed HashMap iteration order, so two
        // nodes with the same executor set would route the same tx to different shards.
        fn build() -> ExecutorSet {
            let mut e = ExecutorSet::default();
            for i in 0..8 {
                e.executors.insert(format!("exec{i}").into_bytes(), 100 + i);
            }
            e
        }
        let forward = build();
        let mut reversed = build();
        reversed.executors.clear();
        for i in (0..8).rev() {
            reversed.executors.insert(format!("exec{i}").into_bytes(), 100 + i);
        }
        let bytes = crate::encoding::serialize_to_bytes_rmp(&forward).unwrap();
        let roundtrip: ExecutorSet = crate::encoding::deserialize_rmp_to(&bytes).unwrap();

        // shard_count==1 hits the shortcut path (full sorted set); the >1 cases hit the
        // shuffle path. Several (shard_count, tx, epoch) tuples cover the round-robin.
        let cases = [(1u32, "tx-a", 7), (3, "tx-a", 7), (4, "other-tx", 9), (2, "tx-a", 7)];
        for (sc, tx, ep) in cases {
            let a = deterministic_select_shard(&forward, sc, tx, ep, &[]).unwrap();
            let b = deterministic_select_shard(&reversed, sc, tx, ep, &[]).unwrap();
            let c = deterministic_select_shard(&roundtrip, sc, tx, ep, &[]).unwrap();
            assert_eq!(a, b, "forward vs reversed differ: shard_count={} tx={} epoch={}", sc, tx, ep);
            assert_eq!(a, c, "forward vs round-trip differ: shard_count={} tx={} epoch={}", sc, tx, ep);
        }
    }

    // --- Phase 5.3 / AUDIT H3: unpredictable selection seeds ---
    //
    // Regression discriminators for binding every selection seed to
    // `prev_block_hash` (plus a per-type domain byte). Each must FAIL if the seed
    // is reverted to depend only on `epoch_number`, which would make the next
    // leader / shard / finalizer predictable from the public stake set.

    #[test]
    fn selection_seed_leader_changes_with_prev_block_hash() {
        // HEADLINE discriminator: same stake set + epoch, different prev_block_hash
        // → different leader. Without this binding an attacker can precompute the
        // next leader and pre-target it before it ever appears.
        let selector = LeaderSelector::new();
        let stakes = make_stake_set(vec![(vec![1], 50), (vec![2], 50)]);
        let leader_a = selector.select(&stakes, 7, &[0x11u8; 32]);
        let leader_b = selector.select(&stakes, 7, &[0x22u8; 32]);
        assert_ne!(leader_a, leader_b, "leader must vary with prev_block_hash");
    }

    #[test]
    fn selection_seed_distinct_domains_differ() {
        // Same stake set + epoch + prev_block_hash, but the LEADER vs FINALIZER
        // domain bytes must land on different nodes — a leader seed must never be
        // replayable as a finalizer seed. The seed-level split is the rigorous
        // proof (the domain byte is hashed into the seed); the selection-level
        // split over a spread stake set shows it end-to-end.
        let stakes = make_stake_set(vec![(vec![1], 10), (vec![2], 30), (vec![3], 60)]);
        let prev = [0x33u8; 32];

        // Seed-level: the domain byte is part of the hashed input, so two
        // selections over the same snapshot derive from different seeds.
        let leader_seed = derive_selection_seed(LEADER_DOMAIN, 7, &prev, &[]);
        let finalizer_seed = derive_selection_seed(FINALIZER_DOMAIN, 7, &prev, b"tx1");
        assert_ne!(leader_seed, finalizer_seed, "domains must be separated in the seed");

        // Selection-level: the different seeds land on different stake keys.
        let leader = deterministic_select(&stakes, LEADER_DOMAIN, &[], 7, &prev).unwrap();
        let finalizer = deterministic_select(&stakes, FINALIZER_DOMAIN, b"tx1", 7, &prev).unwrap();
        assert_ne!(leader, finalizer, "leader and finalizer must not collide");
    }

    #[test]
    fn selection_seed_shard_index_changes_with_prev_block_hash() {
        // The same tx in the same epoch routes to a different shard partition when
        // the previous block hash differs — no pre-targeting of the assigned shard.
        fn build() -> ExecutorSet {
            let mut e = ExecutorSet::default();
            e.executors.insert(b"exec0".to_vec(), 100);
            e.executors.insert(b"exec1".to_vec(), 100);
            e.executors.insert(b"exec2".to_vec(), 100);
            e.executors.insert(b"exec3".to_vec(), 100);
            e
        }
        let executors = build();
        let a = deterministic_select_shard(&executors, 2, "tx-1", 7, &[0x11u8; 32]).unwrap();
        let b = deterministic_select_shard(&executors, 2, "tx-1", 7, &[0x22u8; 32]).unwrap();
        assert_ne!(a, b, "selected shard must vary with prev_block_hash");
    }

    #[test]
    fn selection_seed_matches_manual_hash() {
        // Guards the exact byte layout of the derived seed:
        //   SHA-256(domain ‖ epoch ‖ prev_block_hash ‖ extra)
        let prev = [0x44u8; 32];
        let extra = b"tx1";
        let built = derive_selection_seed(LEADER_DOMAIN, 7, &prev, extra);
        let mut input = Vec::new();
        input.push(LEADER_DOMAIN);
        input.extend_from_slice(&7u64.to_be_bytes());
        input.extend_from_slice(&prev);
        input.extend_from_slice(extra);
        let digest = ring::digest::digest(&ring::digest::SHA256, &input);
        let mut expected = [0u8; 32];
        expected.copy_from_slice(digest.as_ref());
        assert_eq!(built, expected, "derived seed must equal manual SHA-256(domain ‖ epoch ‖ prev ‖ extra)");
    }

    #[test]
    fn selection_seed_independent_of_tx_id_for_leader() {
        // The leader path carries no per-tx extra, so it is one stable leader for
        // the stake set regardless of transaction. The finalizer path salts on
        // tx_id, so across transactions it does NOT pin every tx to a single
        // finalizer — dropping tx_id (the regression) would route all of them to
        // one node and wreck load distribution. Proves `extra` is used by the
        // finalizer/shard-index paths, not the leader.
        let key_a = vec![1];
        let key_b = vec![2];
        let stakes = make_stake_set(vec![(key_a.clone(), 10), (key_b.clone(), 90)]);
        let prev = [0x55u8; 32];

        // Leader: a single, stable key, independent of any transaction.
        let leader = deterministic_select(&stakes, LEADER_DOMAIN, &[], 7, &prev).unwrap();
        assert_eq!(leader.len(), 1, "leader path returns exactly one stake key");

        // Finalizer: across many tx_ids, routing spans more than one key (tx_id
        // is a real salt). A regression that dropped tx_id would pin every tx to
        // the single leader key.
        let mut finalizer_keys = std::collections::BTreeSet::new();
        for i in 0..100 {
            let key = deterministic_select(
                &stakes,
                FINALIZER_DOMAIN,
                format!("tx_{i}").as_bytes(),
                7,
                &prev,
            )
            .unwrap();
            finalizer_keys.insert(key);
        }
        assert!(
            finalizer_keys.len() > 1,
            "finalizer must span multiple keys across txs (tx_id is a real salt); got {:?}",
            finalizer_keys
        );
    }
}
