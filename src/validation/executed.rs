//! `ExecutedBlockValidatorSpec`: token-level + block-level validation for
//! executed tokens (sender/nonce/amount floors, executor+finalizer block
//! requirements).

use super::*;

// ---------------------------------------------------------------------------
// ExecutedBlockValidatorSpec — standard executed/processed blocks
// ---------------------------------------------------------------------------

/// Spec for transactions that go through Executor and Finalizer.
/// Validates that the block was properly executed and signed.
#[derive(Debug, Clone)]
pub struct ExecutedBlockValidatorSpec {
    name: String,
    /// Minimum stake required for the transaction
    min_stake: u64,
}

impl ExecutedBlockValidatorSpec {
    pub fn new(min_stake: u64) -> Self {
        ExecutedBlockValidatorSpec {
            name: String::from("Executed"),
            min_stake,
        }
    }
}

impl Default for ExecutedBlockValidatorSpec {
    fn default() -> Self {
        Self::new(0)
    }
}

impl TransactionValidationSpec for ExecutedBlockValidatorSpec {
    fn validate(
        &self,
        tx: &Transaction,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        // The owner gate now lives only on the SelfSigned path (AUDIT 5.9); the
        // Executed spec is owner-agnostic. `token` is retained for the trait
        // signature but not consulted here.
        let _ = token;

        // Validate basic transaction fields
        let mut failures = Vec::new();

        if tx.sender.is_empty() {
            failures.push(ValidationFailureReason::SenderMissing);
        }

        // Phase 5.6 / M12: amount must be present and nonzero. An `Option<u64>`
        // `None` is rejected at admission (wire-compat: keep it serialized as Option);
        // a zero amount is rejected as before.
        if tx.amount.is_none() || tx.amount == Some(0) {
            failures.push(ValidationFailureReason::InvalidAmount);
        }

        if tx.sequence_number == 0 {
            failures.push(ValidationFailureReason::InvalidNonce);
        }

        if !failures.is_empty() {
            return Err(PneumaticError::Validation(failures));
        }

        let risk = self.calculate_risk(tx);

        // Phase 5.7 / H6: real risk gate — reject the transaction when its
        // composite risk score exceeds the environment's configured max_risk
        // (0.0-1.0). The placeholder that compared risk against
        // override_quorum_percentage (a ~67 quorum value a 0.0-1.0 score can
        // never exceed) is removed.
        if risk.score() > env_data.max_risk {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::RiskExceedsThreshold
            ]));
        }

        // Return the finalizer key — in practice this is assigned by the
        // Sentinel after checking stake thresholds
        Ok(TransactionValidationResult::valid(vec![], risk))
    }

    fn calculate_risk(&self, tx: &Transaction) -> TransactionRiskFactor {
        TransactionRiskFactor {
            affected_parties: if !tx.receiver.is_empty() { 2 } else { 1 },
            amount: tx.amount.unwrap_or(0),
            is_contract: tx.action.contains("Contract"),
            is_multi_party: false,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// BlockValidatorSpec implementation for executed blocks

impl BlockValidatorSpec for ExecutedBlockValidatorSpec {
    fn validate(
        &self,
        block: &crate::blocks::Block,
        _token: &Token,
        _env_data: &EnvironmentMetadata,
    ) -> Result<BlockValidationResult, PneumaticError> {
        // Executed transactions must have a result hash (executor ran)
        if block.signed_trans.transaction.result_hash.is_empty() {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::MissingResultHash,
            ]));
        }

        // Executed transactions must have executor signatures
        if block.signed_trans.executor_sigs.is_empty() {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::MissingExecutorSignatures,
            ]));
        }

        // Executed transactions must have a finalizer signature
        if block.signed_trans.finalizer_sig.signature.is_empty() {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::MissingFinalizerSignature,
            ]));
        }

        Ok(BlockValidationResult::Valid)
    }
}
