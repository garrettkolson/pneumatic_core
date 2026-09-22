//! Terminal node in the pneumatic pipeline — commits blocks to token blockchains,
//! distributes blocks to archivers, and manages epoch transitions (staking,
//! reconciliation, leader selection).

pub mod block_services;
pub mod committer;
pub mod committer_error;
pub mod epoch_manager;
pub mod orphan_buffer;
pub mod shielded_pool;

pub use committer::Committer;
pub use shielded_pool::{PoolApplyOutcome, ShieldedPool};

