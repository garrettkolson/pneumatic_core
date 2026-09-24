//! Validation trait surface: `TransactionValidationSpec` +
//! `BlockValidatorSpec` (+ result enum) and the shielded plumbing traits
//! (`NullifierMembership`, `MerkleRootHistory`).

use super::*;

// ---------------------------------------------------------------------------
// TransactionValidationSpec — action-based validation trait
// ---------------------------------------------------------------------------

/// Trait for validating transactions. Implemented by concrete specs
/// (SelfSigned, Executed, etc.) and registered by action name.
pub trait TransactionValidationSpec: Send + Sync {
    /// Validate a transaction against this spec. Returns a result with
    /// risk metrics and assigned finalizer key.
    fn validate(
        &self,
        tx: &Transaction,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError>;

    /// Compute risk metrics for a transaction.
    fn calculate_risk(&self, tx: &Transaction) -> TransactionRiskFactor;

    /// Return the spec name for registration lookup.
    fn name(&self) -> &str;

    /// Optional shielded-transaction validation. The default impl **fails closed**
    /// (`UnsupportedAction`): a spec that does not override this cannot validate a
    /// shielded tx, so a `"ShieldedTransfer"` reaching an unprepared spec is
    /// rejected (mirrors Phase 3.2's reject-unknown-validator). Only
    /// `ShieldedValidationSpec` overrides it. See Phase S4.1.
    fn validate_shielded(
        &self,
        _tx: &ShieldedTransaction,
        _env_data: &EnvironmentMetadata,
        _deps: &ShieldedValidationDeps,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        Err(PneumaticError::Validation(vec![ValidationFailureReason::UnsupportedAction]))
    }
}

// ---------------------------------------------------------------------------
// BlockValidatorSpec — validates entire blocks (used by Committers/Archivers)
// ---------------------------------------------------------------------------

/// Trait for validating blocks during commit or archiving.
pub trait BlockValidatorSpec: Send + Sync {
    fn validate(
        &self,
        block: &crate::blocks::Block,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<BlockValidationResult, PneumaticError>;
}

#[derive(Debug)]
pub enum BlockValidationResult {
    Valid,
    Invalid(Vec<ValidationFailureReason>),
}

/// Check 2 dependency: the nullifier set's read-side. Implemented by the S4.2
/// `NullifierRegistry`; the spec depends on this trait, never the concrete type,
/// so the check is buildable and testable before the registry exists.
pub trait NullifierMembership {
    fn contains_nullifier(&self, nullifier: [u8; 32]) -> bool;
}

/// Check 3 dependency: the append-only history of committed roots. Implemented by
/// the S4.3 root state.
pub trait MerkleRootHistory {
    /// Roots ordered oldest→newest (the last element is the current tip).
    fn root_history(&self) -> &[RootSnapshot];
}
