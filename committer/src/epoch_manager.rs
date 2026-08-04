use std::sync::Arc;

use dashmap::DashMap;
use rand::Rng;

use pneumatic_core::crypto::HashProvider;
use pneumatic_core::data::DataProvider;
use pneumatic_core::epoch::{EpochReconciliation, IEpochLeaderSelector, IEpochReconciler, IStakingManager, StakeSet, StakingOp};
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
}

impl EpochReconciler {
    pub fn new(data_provider: Arc<dyn DataProvider>, env_id: String) -> Self {
        EpochReconciler { data_provider, env_id }
    }

    /// Run reconciliation — for now returns empty reconciliation data.
    /// TODO: load tokens from data provider, compare chain heads,
    /// detect misshapen tokens and finalization conflicts.
    fn reconcile_internal(&self) -> EpochReconciliation {
        let reconciliation = EpochReconciliation::default();

        // TODO: load each token via data_provider.get_token()
        // for token_id in self.get_known_token_ids() {
        //     let token = self.data_provider.get_token(&token_id, &self.env_id)?;
        //     let chain_state = token.blockchain.get_current_chain_state();
        //     if !chain_state.is_valid {
        //         reconciliation.misshapen_tokens.push(token.id.clone());
        //     }
        // }

        // TODO: detect finalization conflicts by comparing block hashes
        // at the same heights across tokens

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
    /// stake-weighted random selection.
    fn select_internal(&self, stakers: &StakeSet) -> Vec<u8> {
        let total = stakers.total_stake();
        if total == 0 {
            return vec![];
        }

        let mut rng = rand::thread_rng();
        let target: u64 = rng.gen_range(0..total);

        let mut cumulative = 0u64;
        for (key, stake) in &stakers.stakers {
            cumulative += stake;
            if cumulative >= target {
                return key.clone();
            }
        }

        // Fallback: return the first staker (shouldn't happen if total > 0)
        stakers.stakers.keys().next().cloned().unwrap_or_default()
    }
}

impl IEpochLeaderSelector for LeaderSelector {
    fn select(&self, stakers: &StakeSet) -> Vec<u8> {
        self.select_internal(stakers)
    }
}
