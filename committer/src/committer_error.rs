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
    /// The incoming commit's transaction payload differs from the validated/pooled transaction
    /// (AUDIT Phase 3.5 / H12): the block on the wire embeds a transaction that is not the one the
    /// pipeline validated, so the commit is rejected and never appended.
    TransactionPayloadMismatch(String),
    /// Commit message env_id does not match this committer's environment
    EnvironmentMismatch { expected: String, got: String },
    /// Proposed block has an empty hash
    InvalidBlockHash,
    /// The block's finalizer signature is missing or does not verify against the claimed
    /// `finalizer_addr` — fail closed (AUDIT Phase 3.3 / C5).
    InvalidFinalizerSignature,
    /// Block validation/commit failed on the token
    BlockCommit(BlockCommitError),
    /// Unknown message action
    UnknownAction(String),
    /// Sender's Ed25519 public key (hex) is not a registered node — the
    /// committer cannot authenticate the envelope's origin.
    UnauthenticatedSender(String),
    /// Sender is registered but its role is not permitted to send this
    /// action: `<public_key hex>: action=<action> role=<role>`.
    UnauthorizedRole(String),
    /// Underlying PneumaticError from core
    Core(pneumatic_core::errors::PneumaticError),
    /// Internal serialization failure (gossip protocol)
    InternalSerialization,
    /// Gas deduction could not be completed: `get_user`/`save_user` returned a
    /// `DataError`. The committed block stands (it was validated/finalized), but the
    /// sender's fuel balance was not debited, so the transaction does not reach the
    /// `Committed` state — the failure is surfaced rather than silently swallowed
    /// (AUDIT Phase 4.5 / M11: "cannot silently free gas or overdraw").
    GasDeduction {
        sender: String,
        tx_id: String,
        gas_used: u64,
        cause: String,
    },
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

