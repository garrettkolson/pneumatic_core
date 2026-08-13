use serde::{Deserialize, Serialize};
use crate::data::DataError;

/// Top-level error type for all pneumatic protocol failures.
/// Every function returns `Result<_, PneumaticError>` instead of
/// `Option` or `Result<T, Box<dyn Error>>`.
#[derive(Debug)]
pub enum PneumaticError {
    /// Crypto provider errors (encryption, signing, signature verification)
    CryptoError(String),
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
    /// Block proposed by a stale (expired-epoch) leader
    StaleBlock {
        block_hash: Vec<u8>,
        stale_leader: Vec<u8>,
        current_leader: Vec<u8>,
        epoch_number: u64,
    },
    /// Conflicting block proposals at the same height
    BlockConflict {
        height: u64,
        block_a: Vec<u8>,
        block_b: Vec<u8>,
    },
}

impl std::fmt::Display for PneumaticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PneumaticError::CryptoError(msg) => write!(f, "CryptoError({})", msg),
            PneumaticError::Encoding(msg) => write!(f, "Encoding({})", msg),
            PneumaticError::Data(e) => write!(f, "Data({})", e),
            PneumaticError::Network(msg) => write!(f, "Network({})", msg),
            PneumaticError::Validation(reasons) => write!(f, "Validation({:?})", reasons),
            PneumaticError::Registry(msg) => write!(f, "Registry({})", msg),
            PneumaticError::Epoch(msg) => write!(f, "Epoch({})", msg),
            PneumaticError::Block(e) => write!(f, "Block({})", e),
            PneumaticError::StaleBlock { block_hash, stale_leader, current_leader, epoch_number } => {
                write!(f, "StaleBlock {{ block_hash: {:?}, stale_leader: {:?}, current_leader: {:?}, epoch_number: {} }}",
                    block_hash, stale_leader, current_leader, epoch_number)
            }
            PneumaticError::BlockConflict { height, block_a, block_b } => {
                write!(f, "BlockConflict {{ height: {}, block_a: {:?}, block_b: {:?} }}", height, block_a, block_b)
            }
        }
    }
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

impl From<crate::conns::ConnError> for PneumaticError {
    fn from(e: crate::conns::ConnError) -> Self {
        PneumaticError::Network(e.to_string())
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
    /// Sender has zero fuel balance (gas verification failed)
    InsufficientGas,
    /// Sender stake is below the minimum for the required node type
    InsufficientStake,
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
    /// Token is not marked as self-verified (required for self-signed blocks)
    NotSelfVerified,
    /// Token data could not be loaded from the data store
    TokenNotFound,
    /// Executed transaction is missing a result hash (executor did not run)
    MissingResultHash,
    /// Executed transaction has no executor signatures
    MissingExecutorSignatures,
    /// Executed transaction has no finalizer signature
    MissingFinalizerSignature,
    /// Block proposed during a stale epoch (leader epoch expired)
    StaleEpochBlock,
    /// Block conflicts with another proposal at the same height
    BlockConflict,
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

// ---------------------------------------------------------------------------
// Executor routing — shard-aware dispatch errors
// ---------------------------------------------------------------------------

/// Errors that occur when selecting executors for a transaction shard.
/// Pure validation errors — never network or data-provider failures.
#[derive(Debug, Clone)]
pub enum ExecutorRoutingError {
    /// No executors configured for this epoch
    EmptyExecutorSet,
    /// Shard count must be at least 1
    ShardCountZero,
    /// Shard index {0} out of range [0, {1})
    ShardOutOfBounds(u32, u32),
    /// Selected shard has no executors assigned
    NoExecutorsInShard(u32),
}

impl std::fmt::Display for ExecutorRoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorRoutingError::EmptyExecutorSet => write!(f, "EmptyExecutorSet"),
            ExecutorRoutingError::ShardCountZero => write!(f, "ShardCountZero"),
            ExecutorRoutingError::ShardOutOfBounds(shard, max) => {
                write!(f, "ShardOutOfBounds({}, {})", shard, max)
            }
            ExecutorRoutingError::NoExecutorsInShard(shard) => {
                write!(f, "NoExecutorsInShard({})", shard)
            }
        }
    }
}

impl std::error::Error for ExecutorRoutingError {}

impl From<ExecutorRoutingError> for PneumaticError {
    fn from(e: ExecutorRoutingError) -> Self {
        PneumaticError::Epoch(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conns::ConnError;
    use crate::user::User;
    use crate::tokens::Token;
    use crate::errors::ExecutorRoutingError;

    // --- From implementations ---

    #[test]
    fn from_io_error_becomes_encoding() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test io error");
        let err: PneumaticError = io_err.into();
        match err {
            PneumaticError::Encoding(msg) => assert_eq!(msg, "test io error"),
            _ => panic!("expected Encoding variant, got {:?}", err),
        }
    }

    #[test]
    fn from_data_error_becomes_data() {
        let data_err = DataError::DeserializationError(std::io::Error::new(
            std::io::ErrorKind::Other, "test data error",
        ));
        let err: PneumaticError = data_err.into();
        match err {
            PneumaticError::Data(_) => {}
            _ => panic!("expected Data variant, got {:?}", err),
        }
    }

    #[test]
    fn from_block_validation_error_becomes_block() {
        let block_err = BlockValidationError::ImproperBlockFormatting;
        let err: PneumaticError = block_err.into();
        match err {
            PneumaticError::Block(_) => {}
            _ => panic!("expected Block variant, got {:?}", err),
        }
    }

    #[test]
    fn from_conn_error_becomes_network() {
        let conn_err = crate::conns::ConnError::IO("connection lost".to_string());
        let err: PneumaticError = conn_err.into();
        match err {
            PneumaticError::Network(msg) => assert_eq!(msg, "IO(connection lost)"),
            _ => panic!("expected Network variant, got {:?}", err),
        }
    }

    // --- TransactionRiskFactor scoring ---

    #[test]
    fn risk_score_low_amount_one_party_no_complexity() {
        let risk = TransactionRiskFactor {
            affected_parties: 1,
            amount: 0,
            is_contract: false,
            is_multi_party: false,
        };
        // amount_risk=0.0, party_risk=0.0, complexity_risk=0.5
        // score = (0.0 * 0.4) + (0.0 * 0.3) + (0.5 * 0.3) = 0.15
        assert_eq!(risk.score(), 0.15);
    }

    #[test]
    fn risk_score_medium_amount_two_parties() {
        let risk = TransactionRiskFactor {
            affected_parties: 2,
            amount: 500_000,
            is_contract: false,
            is_multi_party: false,
        };
        // amount_risk=0.0 (<=1M), party_risk=0.5, complexity_risk=0.5
        // score = (0.0 * 0.4) + (0.5 * 0.3) + (0.5 * 0.3) = 0.30
        assert_eq!(risk.score(), 0.30);
    }

    #[test]
    fn risk_score_high_amount_three_plus_complex() {
        let risk = TransactionRiskFactor {
            affected_parties: 3,
            amount: 2_000_000_000,
            is_contract: true,
            is_multi_party: false,
        };
        // amount_risk=1.0, party_risk=1.0, complexity_risk=1.0 (is_contract)
        // score = (1.0 * 0.4) + (1.0 * 0.3) + (1.0 * 0.3) = 1.0
        assert_eq!(risk.score(), 1.0);
    }

    #[test]
    fn amount_risk_small_medium_large() {
        assert_eq!(TransactionRiskFactor { affected_parties: 0, amount: 100, is_contract: false, is_multi_party: false }.amount_risk(), 0.0);
        assert_eq!(TransactionRiskFactor { affected_parties: 0, amount: 1_000_001, is_contract: false, is_multi_party: false }.amount_risk(), 0.5);
        assert_eq!(TransactionRiskFactor { affected_parties: 0, amount: 2_000_000_000, is_contract: false, is_multi_party: false }.amount_risk(), 1.0);
    }

    #[test]
    fn party_risk_one_two_three_plus() {
        assert_eq!(TransactionRiskFactor { affected_parties: 1, amount: 0, is_contract: false, is_multi_party: false }.party_risk(), 0.0);
        assert_eq!(TransactionRiskFactor { affected_parties: 2, amount: 0, is_contract: false, is_multi_party: false }.party_risk(), 0.5);
        assert_eq!(TransactionRiskFactor { affected_parties: 5, amount: 0, is_contract: false, is_multi_party: false }.party_risk(), 1.0);
    }

    // --- ValidationFailureReason ---

    #[test]
    fn validation_error_from_failure_reasons() {
        let err = PneumaticError::Validation(vec![ValidationFailureReason::InsufficientFunds]);
        match err {
            PneumaticError::Validation(ref reasons) => {
                assert_eq!(reasons.len(), 1);
                assert!(matches!(reasons[0], ValidationFailureReason::InsufficientFunds));
            }
            _ => panic!("expected Validation variant"),
        }
    }

    #[test]
    fn pneumatic_error_debug_fmt_no_panic() {
        let err = PneumaticError::CryptoError("key expired".to_string());
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("Crypto"));
    }

    #[test]
    fn pneumatic_error_display_crypto_error() {
        let err = PneumaticError::CryptoError("decryption failed".to_string());
        assert!(err.to_string().contains("decryption failed"));
    }

    #[test]
    fn pneumatic_error_from_conn_error_decrypt() {
        let conn_err = ConnError::DecryptError("bad nonce".to_string());
        let pneumatic_err = PneumaticError::from(conn_err);
        match pneumatic_err {
            PneumaticError::Network(msg) => assert!(msg.contains("bad nonce")),
            _ => panic!("expected Network variant"),
        }
    }

    // --- ExecutorRoutingError ---

    #[test]
    fn executor_routing_error_display_empty_set() {
        let err = ExecutorRoutingError::EmptyExecutorSet;
        assert_eq!(err.to_string(), "EmptyExecutorSet");
    }

    #[test]
    fn executor_routing_error_display_shard_out_of_bounds() {
        let err = ExecutorRoutingError::ShardOutOfBounds(5, 3);
        assert_eq!(err.to_string(), "ShardOutOfBounds(5, 3)");
    }

    #[test]
    fn executor_routing_error_converts_to_pneumatic_epoch() {
        let routing_err = ExecutorRoutingError::EmptyExecutorSet;
        let pneumatic_err: PneumaticError = routing_err.into();
        match pneumatic_err {
            PneumaticError::Epoch(msg) => assert!(msg.contains("EmptyExecutorSet")),
            _ => panic!("expected Epoch variant"),
        }
    }
}
