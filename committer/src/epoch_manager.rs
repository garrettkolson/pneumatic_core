use std::sync::Arc;

use dashmap::DashMap;
use rand::Rng;
use rand::SeedableRng;

use pneumatic_core::crypto::HashProvider;
use pneumatic_core::data::DataProvider;
use pneumatic_core::epoch::{Conflict, EpochReconciliation, IEpochLeaderSelector, IEpochReconciler, IStakingManager, StakeSet, StakingOp};
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
/// Initial implementation returns defaults with TODOs for full chain analysis.
/// Replaces `StubEpochReconciler` from core.
pub struct EpochReconciler {
    data_provider: Arc<dyn DataProvider>,
    env_id: String,
    token_ids: Vec<Vec<u8>>,
}

impl EpochReconciler {
    pub fn new(data_provider: Arc<dyn DataProvider>, env_id: String, token_ids: Vec<Vec<u8>>) -> Self {
        EpochReconciler { data_provider, env_id, token_ids }
    }

    /// Run epoch reconciliation — detect misshapen chains and
    /// finalization conflicts across known tokens.
    fn reconcile_internal(&self) -> EpochReconciliation {
        let mut reconciliation = EpochReconciliation::default();

        // Load each token and check chain validity
        for token_id in &self.token_ids {
            match self.data_provider.get_token(token_id, &self.env_id) {
                Ok(token) => {
                    let chain_state = token.blockchain.get_current_chain_state();
                    if !chain_state.is_valid {
                        reconciliation.misshapen_tokens.push(token_id.clone());
                    }
                }
                Err(_) => continue, // token not found, skip
            }
        }

        // Collect valid chains for cross-comparison
        let mut valid_chains: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();

        for token_id in &self.token_ids {
            if let Ok(token) = self.data_provider.get_token(token_id, &self.env_id) {
                let chain_state = token.blockchain.get_current_chain_state();
                if !chain_state.is_valid {
                    continue; // skip misshapen chains
                }
                let hashes: Vec<Vec<u8>> = token.blockchain.chain.iter()
                    .map(|b| b.current_hash.clone())
                    .collect();
                valid_chains.push((token_id.clone(), hashes));
            }
        }

        // Cross-compare: for each pair, check blocks at matching indices
        for i in 0..valid_chains.len() {
            for j in (i + 1)..valid_chains.len() {
                let (_, hashes_i) = &valid_chains[i];
                let (_, hashes_j) = &valid_chains[j];
                let min_len = hashes_i.len().min(hashes_j.len());
                for idx in 0..min_len {
                    if hashes_i[idx] != hashes_j[idx] {
                        reconciliation.finalization_conflicts.push(Conflict {
                            block_a: hashes_i[idx].clone(),
                            block_b: hashes_j[idx].clone(),
                            stake_a: 0, // TODO: resolve from staking state
                            stake_b: 0,
                        });
                    }
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

    use pneumatic_core::blocks::Block;
    use pneumatic_core::data::{DataProvider, StubDataProvider};
    use pneumatic_core::epoch::IEpochReconciler;
    use pneumatic_core::tokens::Token;
    use pneumatic_core::transactions::SignedTransaction;

    use super::EpochReconciler;

    fn build_valid_block(prev_hash: Vec<u8>) -> Block {
        let signed = SignedTransaction::test_transaction();
        let mut block = Block {
            signed_trans: signed,
            token_metadata: std::collections::HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
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
        // Verify chain is valid
        assert!(token.blockchain.get_current_chain_state().is_valid);
        token
    }

    fn make_invalid_token(id: Vec<u8>) -> Token {
        let mut token = make_valid_token(id, 2);
        // Corrupt the chain by breaking hash chaining
        let state = token.blockchain.get_current_chain_state();
        if state.is_valid {
            // Tamper with the last block's hash
            if let Some(block) = token.blockchain.chain.back_mut() {
                block.current_hash = vec![99u8; 32];
            }
        }
        assert!(!token.blockchain.get_current_chain_state().is_valid);
        token
    }

    fn make_reconciler(tokens: Vec<(Vec<u8>, Token)>, token_ids: Vec<Vec<u8>>) -> EpochReconciler {
        let mut stub = StubDataProvider::new();
        for (key, token) in tokens {
            stub = stub.with_token(key, "test".to_string(), token);
        }
        let data_provider: Arc<dyn DataProvider> = Arc::new(stub);
        EpochReconciler::new(data_provider, "test".to_string(), token_ids)
    }

    #[test]
    fn reconcile_empty_token_ids_returns_default() {
        let reconciler = make_reconciler(vec![], vec![]);
        let result = reconciler.reconcile();
        assert!(result.misshapen_tokens.is_empty());
        assert!(result.finalization_conflicts.is_empty());
    }

    #[test]
    fn reconcile_token_not_found_skipped() {
        let reconciler = make_reconciler(vec![], vec![vec![1], vec![2]]);
        let result = reconciler.reconcile();
        assert!(result.misshapen_tokens.is_empty());
        assert!(result.finalization_conflicts.is_empty());
    }

    #[test]
    fn reconcile_valid_chain_not_misshapen() {
        let token = make_valid_token(vec![1], 3);
        let reconciler = make_reconciler(vec![(vec![1], token)], vec![vec![1]]);
        let result = reconciler.reconcile();
        assert!(!result.misshapen_tokens.contains(&vec![1]));
        assert!(result.misshapen_tokens.is_empty());
    }

    #[test]
    fn reconcile_invalid_chain_detected_as_misshapen() {
        let token = make_invalid_token(vec![1]);
        let reconciler = make_reconciler(vec![(vec![1], token)], vec![vec![1]]);
        let result = reconciler.reconcile();
        assert!(result.misshapen_tokens.contains(&vec![1]));
    }

    #[test]
    fn reconcile_conflict_at_matching_height() {
        // Two tokens with same chain height but different genesis hashes
        let mut token_a = Token::new();
        token_a.id = vec![1];
        let block = build_valid_block(vec![1u8; 32]); // different genesis
        token_a.blockchain.add_block(block);

        let token_b = make_valid_token(vec![2], 2); // genesis hash [42; 32]

        let reconciler = make_reconciler(
            vec![(vec![1], token_a), (vec![2], token_b)],
            vec![vec![1], vec![2]],
        );
        let result = reconciler.reconcile();
        assert!(!result.finalization_conflicts.is_empty());
        // Should have at least one conflict at index 0
        let first = &result.finalization_conflicts[0];
        assert_ne!(first.block_a, first.block_b);
    }

    #[test]
    fn reconcile_no_conflict_same_hash() {
        // Two tokens with identical chains at same height
        let token_a = make_valid_token(vec![1], 2);
        let token_b = make_valid_token(vec![2], 2);

        // Token B's chain should have the same hashes since they use the same
        // genesis hash [42; 32] and the same test transaction.
        // But wait — test_block uses a different test_transaction each time?
        // SignedTransaction::test_transaction() is deterministic, so yes.
        let reconciler = make_reconciler(
            vec![(vec![1], token_a), (vec![2], token_b)],
            vec![vec![1], vec![2]],
        );
        let result = reconciler.reconcile();
        // No conflicts because hashes match at all matching indices
        assert!(result.finalization_conflicts.is_empty());
    }

    #[test]
    fn reconcile_different_heights_only_compare_to_min() {
        let token_a = make_valid_token(vec![1], 3); // 3 blocks
        let token_b = make_valid_token(vec![2], 1); // 1 block

        let reconciler = make_reconciler(
            vec![(vec![1], token_a), (vec![2], token_b)],
            vec![vec![1], vec![2]],
        );
        let result = reconciler.reconcile();
        // Only index 0 is compared (min of 3 and 1). Since both use same genesis, no conflict.
        assert!(result.finalization_conflicts.is_empty());
    }
}
