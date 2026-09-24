//! Epoch protocol types: `Epoch`, `StakingOp`, the conflict pair
//! (`Conflict` + `ConflictResolution`), `EpochReconciliation`, the
//! reconciler/staker/leader-selector traits with their stubs, and
//! `resolve_block_conflict`.

use super::*;

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
// Conflict resolution
// ---------------------------------------------------------------------------

/// Resolution outcome from a block conflict — determines how the system responds.
#[derive(Debug, Clone)]
pub enum ConflictResolution {
    /// Different proposers — network race. Commit the winner, discard the loser.
    DiscardLoser(Vec<u8>),
    /// Same proposer signed both blocks — double-signed. Commit the winner, slash the proposer.
    SameProposerSlash(Vec<u8>, Vec<u8>), // (winner_hash, proposer_key_to_slash)
    /// Equal stakes and different proposers — neither verified proposer can be trusted to have
    /// won the fork, so flag both for review and commit neither (fail-closed, AUDIT Phase 5.8 / M10).
    TieFlagBoth(Vec<u8>),
}

/// Resolve a conflict between two block proposals at the same height.
/// Returns a `ConflictResolution` that determines the system response:
/// - **DiscardLoser**: different proposers, network race. Commit winner, discard loser.
/// - **SameProposerSlash**: same proposer double-signed. Commit winner, slash proposer.
/// - **TieFlagBoth**: equal stakes + different proposers. Neither verified proposer can be
///   trusted to have won the fork, so flag both for review and commit neither (fail-closed).
///
/// Equal stakes with different proposers is resolved as `TieFlagBoth` — a deliberately
/// fail-closed outcome. The equal-stake + hash-order tie-break is intentionally absent: block
/// hashes are attacker-searchable, so a hash-order tie-break would be grindable.
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

    // Equal stakes, different proposers — neither verified proposer can be trusted to have
    // won the fork (equal stake means neither is out-staked). Fail closed: flag both for
    // review and commit neither. (The previous lexicographic hash tie-break was
    // attacker-grindable — block hashes are searchable — so it is removed; AUDIT Phase 5.8.)
    Ok(ConflictResolution::TieFlagBoth(block_a_hash.to_vec()))
}
