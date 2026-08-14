use std::io;

use pneumatic_core::tokens::BlockCommitError;

/// Helper: convert bytes to lowercase hex string.
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Errors specific to the Committer crate.
#[derive(Debug)]
pub enum CommitterError {
    /// Serialization/deserialization failure
    Encoding(io::Error),
    /// Failed to deserialize a message body
    Deserialization(io::Error),
    /// Token not found in local cache
    TokenNotFound(String),
    /// Transaction not found in pending registry
    TransactionNotFound(String),
    /// Transaction is not in the expected Finalizing state
    TransactionNotInFinalizing(String),
    /// Commit message env_id does not match this committer's environment
    EnvironmentMismatch { expected: String, got: String },
    /// Proposed block has an empty hash
    InvalidBlockHash,
    /// Block validation/commit failed on the token
    BlockCommit(BlockCommitError),
    /// Unknown message action
    UnknownAction(String),
    /// Underlying PneumaticError from core
    Core(pneumatic_core::errors::PneumaticError),
    /// Internal serialization failure (gossip protocol)
    InternalSerialization,
}

impl From<io::Error> for CommitterError {
    fn from(err: io::Error) -> Self {
        CommitterError::Encoding(err)
    }
}

impl From<pneumatic_core::errors::PneumaticError> for CommitterError {
    fn from(err: pneumatic_core::errors::PneumaticError) -> Self {
        CommitterError::Core(err)
    }
}

impl From<pneumatic_core::tokens::BlockCommitError> for CommitterError {
    fn from(err: pneumatic_core::tokens::BlockCommitError) -> Self {
        CommitterError::BlockCommit(err)
    }
}

