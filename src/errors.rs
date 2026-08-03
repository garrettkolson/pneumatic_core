use serde::{Deserialize, Serialize};
use crate::data::DataError;

/// Top-level error type for all pneumatic protocol failures.
/// Every function returns `Result<_, PneumaticError>` instead of
/// `Option` or `Result<T, Box<dyn Error>>`.
#[derive(Debug)]
pub enum PneumaticError {
    /// Crypto provider errors (encryption, signing, signature verification)
    Crypto(String),
    /// Serialization/deserialization failures
    Encoding(String),
    /// Data provider operation failures
    Data(DataError),
    /// Network/connection failures
    Network(String),
    /// Transaction validation failures with specific reasons
    Validation(Vec<ValidationFailureReason>),
    /// Registry operation failures (add, remove, acquire)
    Registry(String),
    /// Epoch management errors (reconciliation, staking, leader selection)
    Epoch(String),
    /// Block validation failures
    Block(BlockValidationError),
}

impl From<std::io::Error> for PneumaticError {
    fn from(e: std::io::Error) -> Self {
        PneumaticError::Encoding(e.to_string())
    }
}

impl From<DataError> for PneumaticError {
    fn from(e: DataError) -> Self {
        PneumaticError::Data(e)
    }
}

impl From<BlockValidationError> for PneumaticError {
    fn from(e: BlockValidationError) -> Self {
        PneumaticError::Block(e)
    }
}

// ---------------------------------------------------------------------------
// Validation failure reasons (mirrors C# ValidationFailureReason minus dead ones)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ValidationFailureReason {
    /// Transaction sender public key is missing
    SenderMissing,
    /// Transaction sender has insufficient funds for the amount
    InsufficientFunds,
    /// Transaction nonce is invalid (too low, too high, or duplicate)
    InvalidNonce,
    /// Transaction gas balance is insufficient for the computation
    InsufficientGasBalance,
    /// Transaction signature is invalid or unverifiable
    InvalidSignature,
    /// Transaction target contract does not exist
    ContractNotFound,
    /// Transaction amount is invalid (negative, zero for non-transfer)
    InvalidAmount,
    /// Transaction timestamp is outside acceptable bounds
    InvalidTimestamp,
    /// Transaction bid percentage exceeds maximum allowed
    InvalidBidPercentage,
    /// Transaction exceeds maximum allowed gas limit
    GasLimitExceeded,
    /// Transaction risk score exceeds environment threshold
    RiskExceedsThreshold,
    /// Transaction sender is not the token owner
    NotTokenOwner,
    /// Transaction action type is not supported for this token
    UnsupportedAction,
}

// ---------------------------------------------------------------------------
// Risk factor — concrete metrics computed from transaction fields
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionRiskFactor {
    /// Number of affected parties (sender + receiver + any intermediate nodes)
    pub affected_parties: usize,
    /// Transaction amount in smallest units
    pub amount: u64,
    /// Whether the transaction targets a smart contract
    pub is_contract: bool,
    /// Whether the transaction involves multi-party settlement
    pub is_multi_party: bool,
}

impl TransactionRiskFactor {
    /// Compute a composite risk score (0.0 — 1.0).
    /// Higher values indicate higher risk.
    pub fn score(&self) -> f32 {
        let amount_risk = self.amount_risk();
        let party_risk = self.party_risk();
        let complexity_risk = if self.is_contract || self.is_multi_party { 1.0 } else { 0.5 };

        // Weighted average: amount and parties matter most
        (amount_risk * 0.4) + (party_risk * 0.3) + (complexity_risk * 0.3)
    }

    /// Amount risk: 0.0 for small, 0.5 for medium, 1.0 for large.
    pub fn amount_risk(&self) -> f32 {
        if self.amount > 1_000_000_000 {
            1.0
        } else if self.amount > 1_000_000 {
            0.5
        } else {
            0.0
        }
    }

    /// Party risk: 0.0 for 1 party, 0.5 for 2, 1.0 for 3+
    pub fn party_risk(&self) -> f32 {
        match self.affected_parties {
            0..=1 => 0.0,
            2 => 0.5,
            _ => 1.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Reconciled signatures — return type for signature collection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReconciledSignatures {
    /// Merged executor signatures keyed by executor public key
    pub executor_signatures: Vec<ExecutorSignature>,
    /// The finalizer that won quorum
    pub winning_finalizer: Vec<u8>,
    /// Whether a conflict was resolved (supermajority vote or stake-weighted)
    pub conflict_resolved: bool,
}

#[derive(Debug, Clone)]
pub struct ExecutorSignature {
    pub executor_public_key: Vec<u8>,
    pub signature: Vec<u8>,
    pub stake: u64,
}

// ---------------------------------------------------------------------------
// Alias for token block validation error (reuse existing enum)
// ---------------------------------------------------------------------------
pub use crate::tokens::BlockValidationError;
