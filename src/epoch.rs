use serde::{Deserialize, Serialize};
use crate::crypto::sha256;
use crate::encoding::serialize_to_bytes_rmp;
use crate::errors::PneumaticError;
use dashmap::DashMap;
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::Error;
// The pre-split paths `pneumatic_core::epoch::*` survive unchanged:
// the children own the definitions; the parent re-exports them.
pub use self::boundary::EpochBoundaryDetector;
pub use self::candidates::CandidateRegistry;
pub use self::leader::{
    deterministic_select, deterministic_select_shard, derive_selection_seed, LeaderSelector,
    Shuffler, FINALIZER_DOMAIN, LEADER_DOMAIN, SHARD_INDEX_DOMAIN, SHARD_SHUFFLE_DOMAIN,
};
pub use self::proposer::{BlockProposer, IBlockProposer};
pub use self::snapshot_cache::EpochSnapshotCache;
pub use self::stake_sets::{ExecutorSet, StakeSet};
pub use self::types::{
    resolve_block_conflict, Conflict, ConflictResolution, Epoch, EpochReconciliation,
    IEpochLeaderSelector, IEpochReconciler, IStakingManager, StakingOp, StubEpochReconciler,
    StubStakingManager,
};

pub mod boundary;
pub mod candidates;
pub mod leader;
pub mod proposer;
pub mod snapshot_cache;
pub mod stake_sets;
pub mod types;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    pub mod helpers;
    mod boundary;
    mod candidates;
    mod leader;
    mod proposer;
    mod snapshot_cache;
    mod stake_sets;
    mod types;
}
