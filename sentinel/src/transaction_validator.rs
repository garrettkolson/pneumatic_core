use std::sync::Arc;

use pneumatic_core::data::DataProvider;
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
    data_provider: Arc<dyn DataProvider>,
    node_registry: Option<Arc<NodeRegistry>>,
}

impl TransactionValidator {
    pub fn new(env_data: Arc<EnvironmentMetadata>, data_provider: Arc<dyn DataProvider>) -> Self {
        TransactionValidator {
            env_data,
            data_provider,
            node_registry: None,
        }
    }

    pub fn with_registry(mut self, registry: Arc<NodeRegistry>) -> Self {
        self.node_registry = Some(registry);
        self
    }

    /// Validate a transaction against its spec.
    ///
    /// The discriminating signal is the token's `is_self_verified` flag (AUDIT
    /// 5.9): a self-validated (owner-operated) token is governed by the
    /// `SelfSigned` spec, whose only gate is `sender == owner`. Every other token
    /// uses its action's spec (falling back to `Executed`), exactly as before.
    pub fn validate_transaction(
        &self,
        tx: &Transaction,
        _msg: &Message,
    ) -> Result<(), PneumaticError> {
        let specs = &self.env_data.transaction_validation_specs;

        // Load the token first — its `is_self_verified` flag is the genuine
        // discriminator for a self-validated (owner-operated) token.
        let token = match self.data_provider.get_token(&tx.token_id, &self.env_data.token_partition_id) {
            Ok(t) => t,
            Err(_) => return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::TokenNotFound
            ])),
        };

        let spec = if token.is_self_verified {
            // Self-validated token: the owner-check is the only gate.
            match specs.get("SelfSigned") {
                Some(s) => s,
                None => return Err(PneumaticError::Validation(vec![
                    ValidationFailureReason::UnsupportedAction
                ])),
            }
        } else if !tx.action.is_empty() {
            // Standard path: the action's spec, falling back to `Executed`.
            match specs.get(&tx.action) {
                Some(s) => s,
                None => match specs.get("Executed") {
                    Some(s) => s,
                    None => return Err(PneumaticError::Validation(vec![
                        ValidationFailureReason::UnsupportedAction
                    ])),
                },
            }
        } else {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::UnsupportedAction
            ]));
        };

        // Delegate to the selected spec.
        let result = spec.validate(tx, &token, &self.env_data)?;
        if !result.is_valid {
            return Err(PneumaticError::Validation(result.failure_reasons));
        }
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

    /// Compute gas used for a transaction from the cost model.
    /// gas_used = base_cost + (transaction_amount × multiplier_for_action).
    ///
    /// Uses integer fixed-point arithmetic via `CostModel::compute_gas`
    /// to guarantee bitwise-identical results across CPU architectures.
    pub fn compute_gas_used(&self, tx: &Transaction) -> u64 {
        let multiplier = self.env_data.cost_model.amount_multiplier
            .get(&tx.action)
            .copied()
            .unwrap_or(1.0);
        self.env_data.cost_model.compute_gas(tx.amount.unwrap_or(0), multiplier)
    }
}
