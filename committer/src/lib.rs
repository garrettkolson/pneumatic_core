//! Terminal node in the pneumatic pipeline — commits blocks to token blockchains,
//! distributes blocks to archivers, and manages epoch transitions (staking,
//! reconciliation, leader selection).

pub mod block_services;
pub mod committer;
pub mod committer_error;
pub mod epoch_manager;
pub mod orphan_buffer;

pub use committer::Committer;

