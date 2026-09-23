//! Errors specific to Sentinel operations.
//!
//! Extracted from `crate::sentinel` (the committer `committer_error.rs`
//! precedent) so the sentinel's error type lives in its own module.

use pneumatic_core::conns::ConnError;
use pneumatic_core::data::DataError;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::node::NodeRegistryType;


/// Errors specific to Sentinel operations.
#[derive(Debug)]
pub enum SentinelError {
    /// Serialization/deserialization failure
    Encoding(std::io::Error),
    /// Network connection failure
    Connection(ConnError),
    /// Data provider failure
    Data(DataError),
    /// Registry operation failure
    Registry(String),
    /// Transaction already exists in registry
    TransactionAlreadyExists(String),
    /// Transaction is in a terminal state (Committed/Failed)
    TransactionInTerminalState(String),
    /// Transaction is not awaiting finalizer (for rejection handling)
    TransactionNotAwaitingFinalizer(String),
    /// No target nodes of a given type available
    NoTarget(NodeRegistryType),
    /// Unknown action type in incoming message
    UnknownAction(String),
    /// Deterministic routing failure (no snapshot, empty stake set, etc.)
    Routing(String),
    /// The authenticated envelope sender (`message.public_key`) does not match
    /// `transaction.sender` — a peer tried to debit an account whose key it does
    /// not control (AUDIT finding C3).
    UnauthenticatedSubmitter(String),
    /// The transaction's `sender_signature` is missing, empty, or does not verify
    /// against `sender` over the canonical transaction bytes (AUDIT finding C3).
    InvalidSenderSignature(String),
    /// The shielded advisory validation outcome (Phase S5.1): carries the
    /// structured `PneumaticError` (e.g. `Validation([NotSelfVerified])`,
    /// `Validation([NotShieldedOptIn])`, `TokenNotFound`) so callers can match
    /// the exact fail-closed reason. Crate-local — never serialized.
    Validation(PneumaticError),
}

impl From<std::io::Error> for SentinelError {
    fn from(e: std::io::Error) -> Self {
        SentinelError::Encoding(e)
    }
}

impl From<ConnError> for SentinelError {
    fn from(e: ConnError) -> Self {
        SentinelError::Connection(e)
    }
}

impl From<DataError> for SentinelError {
    fn from(e: DataError) -> Self {
        SentinelError::Data(e)
    }
}

impl From<super::transaction_notifier::NotifyError> for SentinelError {
    fn from(e: super::transaction_notifier::NotifyError) -> Self {
        match e {
            super::transaction_notifier::NotifyError::Encoding(inner) => SentinelError::Encoding(inner),
            super::transaction_notifier::NotifyError::Connection(inner) => SentinelError::Connection(inner),
            super::transaction_notifier::NotifyError::Data(inner) => SentinelError::Data(inner),
            super::transaction_notifier::NotifyError::NoTarget(t) => SentinelError::NoTarget(t),
        }
    }
}
