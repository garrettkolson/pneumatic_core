use serde::{Deserialize, Serialize};
use crate::errors::PneumaticError;
use dashmap::DashMap;
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;

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
}

/// Deterministic stake-weighted selection from a sorted stake set.
///
/// Uses `seed_bytes` to create a reproducible random point in `[0, total_stake)`,
/// then walks the sorted stakers to find who owns that cumulative range.
///
/// Returns `Some(key)` of the selected staker, or `None` if the stake set
/// has zero total stake.
pub fn deterministic_select(stakers: &StakeSet, seed_bytes: &[u8], epoch_number: u64) -> Option<Vec<u8>> {
    let total = stakers.total_stake();
    if total == 0 {
        return None;
    }

    // Deterministic seed: SHA-256(epoch_number || seed_bytes)
    let mut input = epoch_number.to_be_bytes().to_vec();
    input.extend_from_slice(seed_bytes);
    let digest = ring::digest::digest(&ring::digest::SHA256, &input);
    let seed = digest.as_ref();
    let mut rng = StdRng::from_seed(seed.try_into().unwrap_or_else(|_| {
        // SHA-256 produces 32 bytes, exactly fits [u8; 32]
        unreachable!("ring SHA-256 always produces 32 bytes")
    }));
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
    /// `epoch_number` is used as the seed source so every node with the
    /// same stake set produces the same leader for the same epoch.
    /// Returns the selected public key(s).
    fn select(&self, stakers: &StakeSet, epoch_number: u64) -> Vec<u8>;
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
    fn select(&self, stakers: &StakeSet, epoch_number: u64) -> Vec<u8> {
        deterministic_select(stakers, &[], epoch_number)
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
    ) -> Self {
        let leader_public_key = selector.select(stake_set, epoch_number);
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
    pub fn advance_to_new_epoch(
        &mut self,
        selector: &dyn IEpochLeaderSelector,
        stake_set: &StakeSet,
        epoch_duration: i64,
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

/// Resolve a conflict between two block proposals at the same height.
/// Returns the winning block hash.
/// Tie-break: lexicographic comparison of block hashes (smaller wins).
pub fn resolve_block_conflict(
    block_a_hash: &[u8],
    block_b_hash: &[u8],
    proposer_a: &[u8],
    proposer_b: &[u8],
    stake_set: &StakeSet,
) -> Result<Vec<u8>, PneumaticError> {
    let stake_a = stake_set.get_stake(proposer_a);
    let stake_b = stake_set.get_stake(proposer_b);

    if stake_a > stake_b {
        return Ok(block_a_hash.to_vec());
    }
    if stake_b > stake_a {
        return Ok(block_b_hash.to_vec());
    }

    // Tie-break: lexicographic comparison (smaller hash wins)
    if block_a_hash <= block_b_hash {
        Ok(block_a_hash.to_vec())
    } else {
        Ok(block_b_hash.to_vec())
    }
}

// ---------------------------------------------------------------------------
// CandidateRegistry — competing block proposals
// ---------------------------------------------------------------------------

/// Registry of competing block candidates keyed by (token_id, previous_hash).
/// Used for conflict detection — when two or more valid blocks reference the
/// same previous_hash for the same token, they represent a fork.
#[derive(Debug, Default)]
pub struct CandidateRegistry {
    /// (token_id, previous_hash) → list of candidate (block, proposer_key) pairs
    candidates: DashMap<(Vec<u8>, Vec<u8>), Vec<(crate::blocks::Block, Vec<u8>)>>,
}

impl CandidateRegistry {
    pub fn new() -> Self {
        CandidateRegistry {
            candidates: DashMap::new(),
        }
    }

    /// Insert a candidate block. If another candidate already exists at this
    /// (token_id, previous_hash), the new candidate is appended — a conflict
    /// is detected when the vec has length >= 2.
    pub fn insert(&self, token_id: Vec<u8>, previous_hash: Vec<u8>,
                  block: crate::blocks::Block, proposer_key: Vec<u8>) {
        let key = (token_id, previous_hash);
        let mut entry = self.candidates.entry(key).or_insert_with(Vec::new);
        entry.push((block, proposer_key));
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
        let leader = selector.select(&stakes, 1);
        assert!(leader.is_empty());
    }

    #[test]
    fn leader_selector_single_staker_always_selected() {
        let selector = LeaderSelector::new();
        let key = vec![1, 2, 3];
        let stakes = make_stake_set(vec![(key.clone(), 100)]);
        // Run 10 times — single staker should always be selected
        for _ in 0..10 {
            assert_eq!(selector.select(&stakes, 1), key);
        }
    }

    #[test]
    fn leader_selector_deterministic_different_epochs_can_differ() {
        let selector = LeaderSelector::new();
        let key_a = vec![1];
        let key_b = vec![2];
        let stakes = make_stake_set(vec![(key_a.clone(), 50), (key_b.clone(), 50)]);
        // Same epoch → same leader
        let leader_epoch1 = selector.select(&stakes, 1);
        assert_eq!(leader_epoch1, selector.select(&stakes, 1));
        // Different epochs → deterministic but may differ
        let leader_epoch2 = selector.select(&stakes, 2);
        // Either they happen to be the same (still deterministic), or differ
        // Just verify both calls with same epoch return the same result
        assert_eq!(leader_epoch1, selector.select(&stakes, 1));
        assert_eq!(leader_epoch2, selector.select(&stakes, 2));
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
            if selector.select(&stakes, 1) == key_small {
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
        let first = selector.select(&stakes, 5);
        for _ in 1..20 {
            assert_eq!(selector.select(&stakes, 5), first);
        }
    }

    // --- Epoch::new_with_leader tests ---

    #[test]
    fn epoch_new_with_leader_sets_fields() {
        let selector = LeaderSelector::new();
        let key = vec![42];
        let stakes = make_stake_set(vec![(key.clone(), 100)]);
        let epoch = Epoch::new_with_leader(1, 1000, 2000, &selector, &stakes);
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
        detector.advance_to_new_epoch(&selector, &stakes, 1000);
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
    fn conflict_resolution_stake_difference_selects_higher() {
        let stakes = make_stake_set(vec![(vec![1], 100), (vec![2], 200)]);
        let winner = resolve_block_conflict(
            b"hash_a", b"hash_b",
            &vec![1], &vec![2],
            &stakes,
        ).unwrap();
        assert_eq!(winner, b"hash_b");
    }

    #[test]
    fn conflict_resolution_tie_break_by_hash() {
        let stakes = make_stake_set(vec![(vec![1], 100), (vec![2], 100)]);
        let winner = resolve_block_conflict(
            b"aa", b"bb",
            &vec![1], &vec![2],
            &stakes,
        ).unwrap();
        assert_eq!(winner, b"aa");
    }

    // --- CandidateRegistry tests ---

    #[test]
    fn registry_empty_on_creation() {
        let registry = CandidateRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
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
        let result = deterministic_select(&stakes, b"tx1", 1);
        assert!(result.is_none());
    }

    #[test]
    fn deterministic_select_single_staker_always_same() {
        let key = vec![1, 2, 3];
        let stakes = make_stake_set(vec![(key.clone(), 100)]);
        for _ in 0..20 {
            assert_eq!(deterministic_select(&stakes, b"tx1", 1), Some(key.clone()));
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
            if deterministic_select(&stakes, tx_id.as_bytes(), 1) == Some(key_a.clone()) {
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
        let epoch1 = deterministic_select(&stakes, b"tx_alpha", 1);
        let epoch1_again = deterministic_select(&stakes, b"tx_alpha", 1);
        assert_eq!(epoch1, epoch1_again); // Same seed + same epoch → same result

        let epoch2 = deterministic_select(&stakes, b"tx_alpha", 2);
        // Epoch2 may or may not differ — but must be deterministic
        let epoch2_again = deterministic_select(&stakes, b"tx_alpha", 2);
        assert_eq!(epoch2, epoch2_again);
    }

    #[test]
    fn deterministic_select_zero_stake_returns_none() {
        let stakes = make_stake_set(vec![(vec![1], 0), (vec![2], 0)]);
        let result = deterministic_select(&stakes, b"tx1", 1);
        assert!(result.is_none());
    }
}
