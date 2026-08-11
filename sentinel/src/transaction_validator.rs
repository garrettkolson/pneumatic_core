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

        let spec = match specs.get(&spec_name) {
            Some(s) => s,
            None => match specs.get("Executed") {
                Some(s) => s,
                None => return Err(PneumaticError::Validation(vec![
                    ValidationFailureReason::UnsupportedAction
                ])),
            },
        };

        // Load the token from the data store to run full spec validation.
        let token = match self.data_provider.get_token(&tx.token_id, &self.env_data.token_partition_id) {
            Ok(t) => t,
            Err(_) => return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::TokenNotFound
            ])),
        };

        // Delegate to the action's validation spec.
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
