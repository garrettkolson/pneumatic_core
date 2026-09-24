//! `TransactionSignatureRegistry`: per-transaction executor-signature
//! registration (tx registration, dedup per executor, removal).

use super::*;

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

    /// Ensure a transaction entry exists in the registry, creating it if absent.
    /// This is the atomic (check-or-create) variant — safe for concurrent callers.
    pub fn ensure_transaction_registered(&self, tx_id: &str) {
        use dashmap::mapref::entry::Entry;
        self.signatures.entry(tx_id.to_string())
            .or_insert_with(HashMap::new);
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
