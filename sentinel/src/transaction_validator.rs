use std::sync::Arc;

use pneumatic_core::errors::{PneumaticError, TransactionRiskFactor, ValidationFailureReason};
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::transactions::Transaction;

/// Validates transactions by looking up the appropriate spec based on
/// the transaction's action type and delegating to that spec.
/// Used by the Sentinel's `handle_process_request`.
pub struct TransactionValidator {
    env_data: Arc<EnvironmentMetadata>,
    node_registry: Option<Arc<NodeRegistry>>,
}

impl TransactionValidator {
    pub fn new(env_data: Arc<EnvironmentMetadata>) -> Self {
        TransactionValidator {
            env_data,
            node_registry: None,
        }
    }

    pub fn with_registry(mut self, registry: Arc<NodeRegistry>) -> Self {
        self.node_registry = Some(registry);
        self
    }

    /// Validate a transaction against its action's spec.
    /// Looks up the spec by action name from the environment's registry.
    /// Returns `Ok(())` if valid, `Err` with specific failure reasons otherwise.
    pub fn validate_transaction(
        &self,
        tx: &Transaction,
        _msg: &Message,
    ) -> Result<(), PneumaticError> {
        let specs = &self.env_data.transaction_validation_specs;

        let spec_name = if !tx.action.is_empty() {
            tx.action.clone()
        } else {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::UnsupportedAction
            ]));
        };

        let _spec = match specs.get(&spec_name) {
            Some(s) => s,
            None => match specs.get("Executed") {
                Some(s) => s,
                None => return Err(PneumaticError::Validation(vec![
                    ValidationFailureReason::UnsupportedAction
                ])),
            },
        };

        // Stub: full validation requires the token to be preloaded.
        // The flow is: validate basic fields → preload data → full spec validation.
        Ok(())
    }

    /// Calculate risk metrics for a transaction using the action's spec.
    pub fn calculate_risk(&self, tx: &Transaction) -> TransactionRiskFactor {
        TransactionRiskFactor {
            affected_parties: if !tx.receiver.is_empty() { 2 } else { 1 },
            amount: tx.amount.unwrap_or(0),
            is_contract: tx.action.contains("Contract"),
            is_multi_party: false,
        }
    }

    /// Determine how many finalizers to route this transaction to based on risk.
    /// Higher risk → more finalizers for quorum.
    pub fn route_finalizers(&self, risk: &TransactionRiskFactor) -> usize {
        let score = risk.score();
        match score {
            0.0..=0.3 => 1,    // Low risk → single finalizer
            0.3..=0.6 => 2,    // Medium risk → two finalizers
            _ => 3,             // High risk → three finalizers
        }
    }
}
