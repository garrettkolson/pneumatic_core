use std::collections::HashMap;
use dashmap::DashMap;
use crate::errors::PneumaticError;
use crate::transactions::{PendingTransaction, TransactionState, TransactionValidationResult, TransactionSignature};

// ---------------------------------------------------------------------------
// PendingTransactionRegistry — manages transactions in-flight
// ---------------------------------------------------------------------------

/// Backed by DashMap for concurrent access. Every method returns `Result`
/// (never `Option`) to distinguish "not found" from "operation failed".
#[derive(Default)]
pub struct PendingTransactionRegistry {
    transactions: DashMap<String, PendingTransaction>,
}

impl PendingTransactionRegistry {
    pub fn new() -> Self {
        PendingTransactionRegistry {
            transactions: DashMap::new(),
        }
    }

    /// Check if a transaction exists in the registry.
    pub fn contains(&self, id: &str) -> bool {
        self.transactions.contains_key(id)
    }

    /// Add a new pending transaction to the registry.
    pub fn add_transaction(&self, id: String, transaction: PendingTransaction) -> Result<(), PneumaticError> {
        if self.transactions.contains_key(&id) {
            return Err(PneumaticError::Registry(format!(
                "Transaction {} already exists in registry", id
            )));
        }
        self.transactions.insert(id, transaction);
        Ok(())
    }

    /// Register a new transaction with initial Pending state.
    /// Callers can use `acquire_transaction` to get mutable access later.
    pub fn register_pending(&self, id: String) -> Result<(), PneumaticError> {
        self.add_transaction(id.clone(), PendingTransaction::new(id, TransactionState::Pending))
    }

    /// Remove a transaction from the registry.
    pub fn remove_transaction(&self, id: &str) -> Result<(), PneumaticError> {
        if self.transactions.remove(id).is_none() {
            return Err(PneumaticError::Registry(format!(
                "Transaction {} not found in registry", id
            )));
        }
        Ok(())
    }

    /// Acquire a lock on a transaction for a new pipeline stage.
    /// Returns Err if the transaction doesn't exist or is in a terminal state.
    pub fn acquire_transaction(&self, id: &str) -> Result<(), PneumaticError> {
        let mut entry = self.transactions.get_mut(id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not found in registry", id
            )))?;
        entry.acquire().map_err(|_| PneumaticError::Registry(format!(
            "Transaction {} in terminal state", id
        )))
    }

    /// Get the validation result for a validated transaction.
    pub fn get_validation_result(&self, id: &str) -> Option<TransactionValidationResult> {
        let entry = self.transactions.get(id)?;
        match &entry.state {
            TransactionState::Validated { validation, .. } => Some(validation.clone()),
            _ => None,
        }
    }

    /// Set the requested finalizer for a transaction awaiting finalizer.
    pub fn set_requested_finalizer(&self, id: &str, finalizer_key: Vec<u8>) -> Result<(), PneumaticError> {
        let mut entry = self.transactions.get_mut(id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not found in registry", id
            )))?;

        // Extract the validated state to avoid borrow conflicts
        let old_state = std::mem::replace(
            &mut entry.state,
            TransactionState::Pending,
        );

        let TransactionState::Validated { transaction, validation } = old_state else {
            // Restore original state
            entry.state = old_state;
            return Err(PneumaticError::Registry(format!(
                "Transaction {} is not validated, cannot set finalizer", id
            )));
        };

        entry.transition_to_finalizing(transaction, finalizer_key.clone());
        // validation.finalizer_public_key = finalizer_key;
        // (validation result is stored separately; finalizer_key is set in state)
        Ok(())
    }

    /// Check if a transaction's finalizer matches the expected key.
    pub fn is_requested_finalizer(&self, id: &str, expected_key: &[u8]) -> bool {
        let entry = match self.transactions.get_mut(id) {
            Some(e) => e,
            None => return false,
        };
        match &entry.state {
            TransactionState::Finalizing { finalizer_key, .. } => finalizer_key == expected_key,
            _ => false,
        }
    }

    /// Release a lock on a transaction. Returns true if the transaction
    /// should be removed from the registry.
    pub fn release_transaction(&self, id: &str) -> Result<bool, PneumaticError> {
        let mut entry = self.transactions.get_mut(id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not found in registry", id
            )))?;
        Ok(entry.release())
    }

    /// Check if any transaction is awaiting finalizer assignment.
    pub fn transaction_is_awaiting_finalizer(&self, id: &str) -> bool {
        let entry = match self.transactions.get_mut(id) {
            Some(e) => e,
            None => return false,
        };
        entry.is_awaiting_finalizer()
    }
}

// ---------------------------------------------------------------------------
// TransactionSignatureRegistry — tracks executor signatures per transaction
// ---------------------------------------------------------------------------

/// Signature collection only — no quorum logic, no block building.
/// Used by the Finalizer's SignatureCollector component.
#[derive(Default)]
pub struct TransactionSignatureRegistry {
    /// Signatures keyed by transaction ID, then by executor public key
    signatures: DashMap<String, HashMap<Vec<u8>, TransactionSignature>>,
}

impl TransactionSignatureRegistry {
    pub fn new() -> Self {
        TransactionSignatureRegistry {
            signatures: DashMap::new(),
        }
    }

    /// Try to add a new transaction entry (first time seeing this tx).
    pub fn try_add_transaction(&self, tx_id: &str) -> Result<(), PneumaticError> {
        if self.signatures.contains_key(tx_id) {
            return Err(PneumaticError::Registry(format!(
                "Transaction {} already in signature registry", tx_id
            )));
        }
        self.signatures.insert(tx_id.to_string(), HashMap::new());
        Ok(())
    }

    /// Check if a transaction is already registered for signatures.
    pub fn transaction_is_registered(&self, tx_id: &str) -> bool {
        self.signatures.contains_key(tx_id)
    }

    /// Get the signature map for a transaction.
    pub fn get_transaction_registry(&self, tx_id: &str) -> Option<HashMap<Vec<u8>, TransactionSignature>> {
        self.signatures.get(tx_id).map(|map| map.clone())
    }

    /// Try to add an executor signature for a transaction.
    pub fn try_add_signature(
        &self,
        tx_id: &str,
        executor_key: Vec<u8>,
        signature: TransactionSignature,
    ) -> Result<(), PneumaticError> {
        let mut map = self.signatures.get_mut(tx_id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not in signature registry", tx_id
            )))?;
        if map.contains_key(&executor_key) {
            return Err(PneumaticError::Registry(format!(
                "Duplicate signature from executor {:?} for transaction {}",
                executor_key, tx_id
            )));
        }
        map.insert(executor_key, signature);
        Ok(())
    }

    /// Try to remove a transaction entry (cleanup after commit).
    pub fn try_remove_transaction(&self, tx_id: &str) -> Result<(), PneumaticError> {
        if self.signatures.remove(tx_id).is_none() {
            return Err(PneumaticError::Registry(format!(
                "Transaction {} not in signature registry", tx_id
            )));
        }
        Ok(())
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.signatures.is_empty()
    }

    /// Count of registered transactions.
    pub fn len(&self) -> usize {
        self.signatures.len()
    }
}
