use crate::errors::PneumaticError;

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
#[derive(Debug, Default)]
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
    /// Select leader(s) from the current stake set
    /// Returns the selected public key(s)
    fn select(&self, stakers: &StakeSet) -> Vec<u8>;
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

/// Stub leader selector that returns an empty key.
/// Replace with real stake-weighted random selection in Phase 5.
pub struct StubLeaderSelector;

impl IEpochLeaderSelector for StubLeaderSelector {
    fn select(&self, _stakers: &StakeSet) -> Vec<u8> {
        vec![]
    }
}
