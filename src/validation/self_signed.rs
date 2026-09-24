//! `SelfSignedBlockValidatorSpec`: token-level + block-level validation
//! for self-verified tokens (owner-derived sender check, Ed25519 keys).

use super::*;

// ---------------------------------------------------------------------------
// SelfSignedBlockValidatorSpec — self-validated tokens
// ---------------------------------------------------------------------------

/// Spec for tokens where the owner IS the transaction authority.
/// Transactions pass validation without Executor or Finalizer involvement.
/// Sets `is_self_verified = true` on the token.
#[derive(Debug, Clone)]
pub struct SelfSignedBlockValidatorSpec {
    name: String,
}

impl SelfSignedBlockValidatorSpec {
    pub fn new() -> Self {
        SelfSignedBlockValidatorSpec {
            name: String::from("SelfSigned"),
        }
    }
}

impl Default for SelfSignedBlockValidatorSpec {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionValidationSpec for SelfSignedBlockValidatorSpec {
    fn validate(
        &self,
        tx: &Transaction,
        token: &Token,
        _env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        // Check that the transaction sender is the token owner. The owner is
        // stored as a hex string in metadata (AUDIT 5.9), so decode it to the
        // real key bytes and compare against `tx.sender`. A missing owner fails
        // closed; an unparseable owner also fails closed as NotTokenOwner — never
        // `unwrap`/`from_utf8` on the decode path, since a real 32-byte Ed25519
        // key is not valid UTF-8.
        let token_owner = match token.metadata.get("owner").map(|o| o.as_str()) {
            Some(hex_owner) => hex::decode(hex_owner)
                .map_err(|_| PneumaticError::Validation(vec![ValidationFailureReason::NotTokenOwner]))?,
            None => return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::NotTokenOwner
            ])),
        };

        if tx.sender != token_owner {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::NotTokenOwner
            ]));
        }

        let risk = self.calculate_risk(tx);
        // Self-signed tokens have no finalizer — empty key signals skip
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

// BlockValidatorSpec implementation for self-signed blocks

impl BlockValidatorSpec for SelfSignedBlockValidatorSpec {
    fn validate(
        &self,
        block: &crate::blocks::Block,
        token: &Token,
        _env_data: &EnvironmentMetadata,
    ) -> Result<BlockValidationResult, PneumaticError> {
        // Validate chain integrity (delegate to the token's blockchain)
        if !token.blockchain.validate_next_block(&block) {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::NotSelfVerified,
            ]));
        }

        // Self-signed tokens must be flagged as self-verified
        if !token.is_self_verified {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::NotSelfVerified,
            ]));
        }

        Ok(BlockValidationResult::Valid)
    }
}
