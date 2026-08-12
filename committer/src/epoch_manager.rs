use std::sync::Arc;

use dashmap::DashMap;
use rand::{Rng, SeedableRng};

use pneumatic_core::crypto::HashProvider;
use pneumatic_core::data::DataProvider;
use pneumatic_core::epoch::{CandidateRegistry, Conflict, EpochReconciliation, IEpochLeaderSelector, IEpochReconciler, IStakingManager, StakeSet, StakingOp};
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::logging::Logger;

// ---------------------------------------------------------------------------
// StakeStore — concrete stake state backed by DashMap
// ---------------------------------------------------------------------------

/// Concurrent stake storage that replaces the no-op StubStakingManager.
/// Backed by a DashMap for lock-free concurrent access.
pub struct StakeStore {
    stakes: Arc<DashMap<Vec<u8>, u64>>,
}

impl StakeStore {
    pub fn new() -> Self {
        StakeStore {
            stakes: Arc::new(DashMap::new()),
        }
    }

    /// Add or update a staker with their stake amount.
    pub fn add_staker(&self, key: Vec<u8>, stake: u64) {
        self.stakes.insert(key, stake);
    }

    /// Remove a staker by public key.
    pub fn remove_staker(&self, key: &[u8]) {
        self.stakes.remove(key);
    }

    /// Slash a staker's stake by the given amount.
    pub fn slash(&self, key: &[u8], amount: u64) {
        if let Some(mut entry) = self.stakes.get_mut(key) {
            let new_stake = entry.saturating_sub(amount);
            *entry = new_stake;
        }
    }

    /// Reward a staker with additional stake.
    pub fn reward(&self, key: &[u8], amount: u64) {
        if let Some(mut entry) = self.stakes.get_mut(key) {
            *entry += amount;
        }
    }

    /// Get the current stake for a public key.
    pub fn get_stake(&self, key: &[u8]) -> u64 {
        self.stakes.get(key).map(|e| *e.value()).unwrap_or(0)
    }

    /// Iterate over all stakers.
    pub fn iter(&self) -> impl Iterator<Item = (Vec<u8>, u64)> + '_ {
        self.stakes.iter().map(|e| (e.key().clone(), *e.value()))
    }

    /// Convert to a StakeSet for leader selection.
    pub fn to_stake_set(&self) -> StakeSet {
        StakeSet {
            stakers: self.stakes.iter().map(|e| (e.key().clone(), *e.value())).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// StakingManager — applies reconciliation ops to concrete storage
// ---------------------------------------------------------------------------

/// Applies staking operations from epoch reconciliation to the `StakeStore`.
/// Replaces the no-op `StubStakingManager` from core.
pub struct StakingManager {
    stake_store: Arc<StakeStore>,
    logger: Arc<dyn Logger>,
}

impl StakingManager {
    pub fn new(stake_store: Arc<StakeStore>, logger: Arc<dyn Logger>) -> Self {
        StakingManager { stake_store, logger }
    }
}

impl IStakingManager for StakingManager {
    fn apply_ops(&self, ops: &EpochReconciliation) -> Result<(), PneumaticError> {
        let op_count = ops.slashing_ops.len() + ops.reward_ops.len();

        for op in &ops.slashing_ops {
            match op {
                StakingOp::Slash(key, amount) => {
                    self.stake_store.slash(key, *amount);
                }
                StakingOp::Reward(key, amount) => {
                    self.stake_store.reward(key, *amount);
                }
                StakingOp::AddStaker(key, stake) => {
                    self.stake_store.add_staker(key.clone(), *stake);
                }
                StakingOp::RemoveStaker(key) => {
                    self.stake_store.remove_staker(key);
                }
            }
        }

        for op in &ops.reward_ops {
            match op {
                StakingOp::Slash(key, amount) => {
                    self.stake_store.slash(key, *amount);
                }
                StakingOp::Reward(key, amount) => {
                    self.stake_store.reward(key, *amount);
                }
                StakingOp::AddStaker(key, stake) => {
                    self.stake_store.add_staker(key.clone(), *stake);
                }
                StakingOp::RemoveStaker(key) => {
                    self.stake_store.remove_staker(key);
                }
            }
        }

        if op_count > 0 {
            self.logger.log(format!("Applied {} staking ops during epoch reconciliation", op_count));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EpochReconciler — examines chain state at epoch boundaries
// ---------------------------------------------------------------------------

/// Examines chain state and returns staking/conflict operations to apply.
/// Uses CandidateRegistry for same-chain conflict detection and StakeStore
/// for real stake resolution (Phase 2 of Protocol Rearchitecture).
pub struct EpochReconciler {
    stake_store: Arc<StakeStore>,
    candidate_registry: Arc<CandidateRegistry>,
    data_provider: Arc<dyn DataProvider>,
    env_id: String,
    token_ids: Vec<Vec<u8>>,
}

impl EpochReconciler {
    pub fn new(
        stake_store: Arc<StakeStore>,
        candidate_registry: Arc<CandidateRegistry>,
        data_provider: Arc<dyn DataProvider>,
        env_id: String,
        token_ids: Vec<Vec<u8>>,
    ) -> Self {
        EpochReconciler {
            stake_store,
            candidate_registry,
            data_provider,
            env_id,
            token_ids,
        }
    }

    /// Run epoch reconciliation — detect misshapen chains and
    /// same-chain fork conflicts via CandidateRegistry.
    ///
    /// For same-chain detection: a conflict exists when the CandidateRegistry
    /// holds 2+ candidates at the same `(token_id, previous_hash)` — meaning
    /// two proposers built on the same parent block.
    fn reconcile_internal(&self) -> EpochReconciliation {
        // Build a StakeSet from StakeStore for conflict resolution
        let mut stake_set = StakeSet {
            stakers: self.stake_store.iter()
                .map(|(key, stake)| (key, stake))
                .collect(),
        };

        let mut reconciliation = EpochReconciliation::default();

        // Load each token and check chain validity / conflicts
        for token_id in &self.token_ids {
            let token = match self.data_provider.get_token(token_id, &self.env_id) {
                Ok(token) => token,
                Err(_) => continue, // token not found, skip
            };

            // Check chain validity
            let chain_state = token.blockchain.get_current_chain_state();
            if !chain_state.is_valid {
                reconciliation.misshapen_tokens.push(token_id.clone());
                continue;
            }

            // Check CandidateRegistry for same-chain conflicts at the tip
            let tip_hash = chain_state.last_hash_in;
            let candidate_count = self.candidate_registry.candidate_count(token_id, &tip_hash);
            if candidate_count >= 2 {
                let candidates = self.candidate_registry.get_candidates(token_id, &tip_hash);
                for pair in candidates.windows(2) {
                    let (block_a, proposer_a) = &pair[0];
                    let (block_b, proposer_b) = &pair[1];

                    let stake_a = self.stake_store.get_stake(&proposer_a);
                    let stake_b = self.stake_store.get_stake(&proposer_b);

                    stake_set.stakers.insert(proposer_a.clone(), stake_a);
                    stake_set.stakers.insert(proposer_b.clone(), stake_b);

                    reconciliation.finalization_conflicts.push(Conflict {
                        block_a: block_a.current_hash.clone(),
                        block_b: block_b.current_hash.clone(),
                        stake_a,
                        stake_b,
                    });
                }
            }
        }

        reconciliation
    }
}

impl IEpochReconciler for EpochReconciler {
    fn reconcile(&self) -> EpochReconciliation {
        self.reconcile_internal()
    }
}

// ---------------------------------------------------------------------------
// LeaderSelector — stake-weighted random leader selection
// ---------------------------------------------------------------------------

/// Selects the block leader for an epoch using stake-weighted random
/// selection. Replaces `StubLeaderSelector` from core.
pub struct LeaderSelector {
    hash_provider: Arc<dyn HashProvider>,
}

impl LeaderSelector {
    pub fn new(hash_provider: Arc<dyn HashProvider>) -> Self {
        LeaderSelector { hash_provider }
    }

    /// Select leader(s) from the current stake set using
    /// stake-weighted deterministic selection.
    fn select_internal(&self, stakers: &StakeSet, epoch_number: u64) -> Vec<u8> {
        let total = stakers.total_stake();
        if total == 0 {
            return vec![];
        }

        // Deterministic seed: hash(epoch_number) using the provided hash provider
        let seed = self.hash_provider.hash(&epoch_number.to_be_bytes());
        let mut rng = rand::rngs::StdRng::from_seed(seed.try_into().unwrap_or_else(|_| {
            // SHA-256 produces 32 bytes, exactly fits [u8; 32]
            unreachable!("hash provider always produces 32 bytes")
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
                return key.clone();
            }
        }
        // Fallback: return the first staker (shouldn't happen if total > 0)
        first_key
    }
}

impl IEpochLeaderSelector for LeaderSelector {
    fn select(&self, stakers: &StakeSet, epoch_number: u64) -> Vec<u8> {
        self.select_internal(stakers, epoch_number)
    }
}

// ---------------------------------------------------------------------------
// EpochReconciler tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pneumatic_core::blocks::{Block, FinalityStatus};
    use pneumatic_core::data::{DataProvider, StubDataProvider};
    use pneumatic_core::epoch::{CandidateRegistry, IEpochReconciler};
    use pneumatic_core::tokens::Token;
    use pneumatic_core::transactions::SignedTransaction;

    use super::{EpochReconciler, StakeStore};

    fn build_valid_block(prev_hash: Vec<u8>) -> Block {
        let signed = SignedTransaction::test_transaction();
        let mut block = Block {
            signed_trans: signed,
            token_metadata: std::collections::HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
        };
        block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block);
        block
    }

    fn make_valid_token(id: Vec<u8>, chain_count: usize) -> Token {
        let mut token = Token::new();
        token.id = id;
        let mut prev_hash = vec![42u8; 32]; // genesis hash
        for _ in 0..chain_count {
            let block = build_valid_block(prev_hash);
            prev_hash = block.current_hash.clone();
            token.blockchain.add_block(block);
        }
        assert!(token.blockchain.get_current_chain_state().is_valid);
        token
    }

    fn make_invalid_token(id: Vec<u8>) -> Token {
        let mut token = make_valid_token(id, 2);
        if let Some(block) = token.blockchain.chain.back_mut() {
            block.current_hash = vec![99u8; 32];
        }
        assert!(!token.blockchain.get_current_chain_state().is_valid);
        token
    }

    fn make_stake_store(stakes: Vec<(Vec<u8>, u64)>) -> Arc<StakeStore> {
        let store = Arc::new(StakeStore::new());
        for (key, stake) in stakes {
            store.add_staker(key, stake);
        }
        store
    }

    fn make_reconciler(
        stakes: Vec<(Vec<u8>, u64)>,
        tokens: Vec<(Vec<u8>, Token)>,
        token_ids: Vec<Vec<u8>>,
    ) -> (EpochReconciler, Arc<StakeStore>, Arc<CandidateRegistry>) {
        let stake_store = make_stake_store(stakes);
        let registry = Arc::new(CandidateRegistry::new());
        let mut stub = StubDataProvider::new();
        for (key, token) in tokens {
            stub = stub.with_token(key, "test".to_string(), token);
        }
        let data_provider: Arc<dyn DataProvider> = Arc::new(stub);
        let reconciler = EpochReconciler::new(
            stake_store.clone(),
            registry.clone(),
            data_provider,
            "test".to_string(),
            token_ids,
        );
        (reconciler, stake_store, registry)
    }

    // --- Basic reconciliation ---

    #[test]
    fn reconcile_empty_token_ids_returns_default() {
        let (reconciler, _, _) = make_reconciler(vec![], vec![], vec![]);
        let result = reconciler.reconcile();
        assert!(result.misshapen_tokens.is_empty());
        assert!(result.finalization_conflicts.is_empty());
    }

    #[test]
    fn reconcile_token_not_found_skipped() {
        let (reconciler, _, _) = make_reconciler(vec![], vec![], vec![vec![1], vec![2]]);
        let result = reconciler.reconcile();
        assert!(result.misshapen_tokens.is_empty());
        assert!(result.finalization_conflicts.is_empty());
    }

    #[test]
    fn reconcile_valid_chain_not_misshapen() {
        let token = make_valid_token(vec![1], 3);
        let (reconciler, _, _) = make_reconciler(vec![], vec![(vec![1], token)], vec![vec![1]]);
        let result = reconciler.reconcile();
        assert!(!result.misshapen_tokens.contains(&vec![1]));
        assert!(result.misshapen_tokens.is_empty());
    }

    #[test]
    fn reconcile_invalid_chain_detected_as_misshapen() {
        let token = make_invalid_token(vec![1]);
        let (reconciler, _, _) = make_reconciler(vec![], vec![(vec![1], token)], vec![vec![1]]);
        let result = reconciler.reconcile();
        assert!(result.misshapen_tokens.contains(&vec![1]));
    }

    // --- Same-chain conflict detection via CandidateRegistry ---

    #[test]
    fn reconcile_no_candidates_no_conflicts() {
        let (reconciler, _, _) = make_reconciler(
            vec![(vec![1], 100)],
            vec![(vec![1], make_valid_token(vec![1], 2))],
            vec![vec![1]],
        );
        // No candidates inserted → no conflicts
        let result = reconciler.reconcile();
        assert!(result.finalization_conflicts.is_empty());
    }

    #[test]
    fn reconcile_single_candidate_no_conflict() {
        let (reconciler, store, registry) = make_reconciler(
            vec![(vec![1], 100)],
            vec![(vec![1], make_valid_token(vec![1], 2))],
            vec![vec![1]],
        );
        let tip_hash = tip_hash_for(&reconciler);
        let block = build_valid_block(vec![1, 2, 3]);
        store.add_staker(vec![1], 100);
        registry.insert(vec![1], tip_hash, block, vec![1]);
        let result = reconciler.reconcile();
        assert!(result.finalization_conflicts.is_empty());
    }

    // --- Helper to get tip hash from first valid token ---

    fn tip_hash_for(reconciler: &EpochReconciler) -> Vec<u8> {
        for token_id in &reconciler.token_ids {
            if let Ok(token) = reconciler.data_provider.get_token(token_id, &reconciler.env_id) {
                let state = token.blockchain.get_current_chain_state();
                if state.is_valid && !state.last_hash_in.is_empty() {
                    return state.last_hash_in;
                }
            }
        }
        vec![0u8; 32]
    }

    #[test]
    fn reconcile_conflict_at_tip_detected() {
        let (reconciler, store, registry) = make_reconciler(
            vec![(vec![1], 100)],
            vec![(vec![1], make_valid_token(vec![1], 2))],
            vec![vec![1]],
        );
        let tip_hash = tip_hash_for(&reconciler);
        let block_a = build_valid_block(vec![1, 2, 3]);
        let block_b = build_valid_block(vec![4, 5, 6]);
        store.add_staker(vec![1], 100);
        store.add_staker(vec![2], 200);
        registry.insert(vec![1], tip_hash.clone(), block_a, vec![1]);
        registry.insert(vec![1], tip_hash, block_b, vec![2]);

        let result = reconciler.reconcile();
        assert_eq!(result.finalization_conflicts.len(), 1);
        let conflict = &result.finalization_conflicts[0];
        assert_ne!(conflict.block_a, conflict.block_b);
        assert_eq!(conflict.stake_a, 100);
        assert_eq!(conflict.stake_b, 200);
    }

    #[test]
    fn reconcile_conflict_stake_resolution_returns_real_stakes() {
        let (reconciler, store, registry) = make_reconciler(
            vec![(vec![1], 500)],
            vec![(vec![1], make_valid_token(vec![1], 2))],
            vec![vec![1]],
        );
        let tip_hash = tip_hash_for(&reconciler);
        let block_a = build_valid_block(vec![10]);
        let block_b = build_valid_block(vec![20]);
        store.add_staker(vec![1], 100);
        store.add_staker(vec![2], 500);
        registry.insert(vec![1], tip_hash.clone(), block_a, vec![1]);
        registry.insert(vec![1], tip_hash, block_b, vec![2]);

        let result = reconciler.reconcile();
        assert_eq!(result.finalization_conflicts.len(), 1);
        let conflict = &result.finalization_conflicts[0];
        // Proposer 2 has more stake → should have higher stake in conflict
        assert_eq!(conflict.stake_a, 100);
        assert_eq!(conflict.stake_b, 500);
    }

    // --- Concurrent access ---

    #[test]
    fn reconcile_concurrent_token_access_no_panic() {
        let stakes: Vec<(Vec<u8>, u64)> = (0..5).map(|i| (vec![i], 100)).collect();
        let tokens: Vec<(Vec<u8>, Token)> = (0..5).map(|i| {
            (vec![i], make_valid_token(vec![i], 2))
        }).collect();
        let token_ids: Vec<Vec<u8>> = (0..5).map(|i| vec![i]).collect();
        let (reconciler, _, _) = make_reconciler(stakes, tokens, token_ids);
        let result = std::thread::scope(|s| {
            let mut handles = vec![];
            for _ in 0..10 {
                let r = &reconciler;
                handles.push(s.spawn(|| r.reconcile()));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });
        // All joins succeeded → no data races
        for res in &result {
            assert!(res.misshapen_tokens.is_empty());
        }
    }
}
