//! Pneumatic Sentinel — transaction gate, validation, and routing.
//!
//! The Sentinel is the first node in the pipeline. It:
//! - Receives raw transactions from senders
//! - Validates them against the appropriate `TransactionValidationSpec`
//! - Routes validated transactions through the pipeline (Executor → Finalizer or direct to Committer for self-signed)
//! - Manages transaction state in the `PendingTransactionRegistry`

pub mod sentinel;
pub mod executor_set_cache;
pub mod stake_snapshot_cache;
pub mod transaction_validator;
pub mod transaction_notifier;

pub use sentinel::Sentinel;
pub use executor_set_cache::ExecutorSetCache;
pub use stake_snapshot_cache::StakeSnapshotCache;
pub use transaction_validator::TransactionValidator;
pub use transaction_notifier::TransactionNotifier;
