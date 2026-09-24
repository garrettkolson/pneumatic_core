//! `PendingTransactionRegistry`: in-flight transaction state — the
//! pending-entry ops, shielded entries, requested-finalizer marking, the
//! per-token pools, and the gas + admin-credit side tables.

use super::*;

impl PendingTransactionRegistry {
    pub fn new() -> Self {
        PendingTransactionRegistry {
            transactions: DashMap::new(),
            pool: Mutex::new(TransactionPool::new()),
            admin_credits: DashMap::new(),
            gas_tracker: Mutex::new(HashMap::new()),
            used_nonces: DashMap::new(),
            shielded_transactions: DashMap::new(),
        }
    }

    /// Check if a transaction exists in the registry.
    pub fn contains(&self, id: &str) -> bool {
        self.transactions.contains_key(id)
    }

    /// Add a new pending transaction to the registry.
    pub fn add_transaction(&self, id: String, transaction: PendingTransaction) -> Result<(), PneumaticError> {
        // `insert` is atomic: it returns the old value if the key was present.
        // Using the return value instead of a prior `contains_key` check
        // avoids a TOCTOU race under concurrent inserts of the same id.
        let id_clone = id.clone();
        if self.transactions.insert(id, transaction).is_some() {
            return Err(PneumaticError::Registry(format!(
                "Transaction {} already exists in registry", id_clone
            )));
        }
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

    // ------------------------------------------------------------------
    // Shielded transfers (Phase S5.1) — a parallel, never-evicted map.
    //
    // Shielded txs have no `TransactionState` lifecycle: they are admitted
    // once, signalled to the finalizer, and stay in the registry permanently
    // (the committer's S5.3 hash-match reads them at block-commit time,
    // which happens after the tx is long out of any pending pipeline).
    // There is deliberately **no remove path** here — eviction is a
    // double-spend/consensus bug (roadmap 2.5), the same reason
    // `used_nonces` never evicts.
    // ------------------------------------------------------------------

    /// Admit a validated shielded transfer into the registry.
    ///
    /// Atomic like `add_transaction`: `insert` returns the old value, so a
    /// duplicate id is detected without a TOCTOU window. A duplicate id is a
    /// protocol violation (the same id was already admitted) and is rejected,
    /// never silently overwritten — overwriting would let a second,
    /// differently-proven tx squat an id the first one was already signed.
    pub fn register_shielded(&self, tx: &ShieldedTransaction) -> Result<(), PneumaticError> {
        // Atomic like `add_transaction`: `insert` returns the old value, so
        // the duplicate check has no TOCTOU window. But unlike a pending tx,
        // an admitted shielded tx may never be *replaced*: the finalizer has
        // already signed it and the committer will hash-match against it at
        // block-commit time, so a duplicate submission must not be able to
        // transiently swap a signed id for a new payload. If the key was
        // present, restore the original value before rejecting.
        let prev = self.shielded_transactions.insert(tx.id.clone(), tx.clone());
        if let Some(old) = prev {
            self.shielded_transactions.insert(tx.id.clone(), old);
            return Err(PneumaticError::Registry(format!(
                "ShieldedTransaction {} already exists in registry", tx.id
            )));
        }
        Ok(())
    }

    /// Is a shielded transfer admitted in the registry?
    pub fn contains_shielded(&self, id: &str) -> bool {
        self.shielded_transactions.contains_key(id)
    }

    /// Fetch a registered shielded transfer (cloned — the registry owns the
    /// canonical copy; callers may not mutate it).
    pub fn get_shielded(&self, id: &str) -> Option<ShieldedTransaction> {
        self.shielded_transactions.get(id).map(|e| e.value().clone())
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
    /// Returns `Err` if the transaction doesn't exist or isn't in Validated state.
    pub fn get_validation_result(&self, id: &str) -> Result<TransactionValidationResult, PneumaticError> {
        let entry = self.transactions.get(id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not found in registry", id
            )))?;
        match &entry.state {
            TransactionState::Validated { validation, .. } => Ok(validation.clone()),
            _ => Err(PneumaticError::Registry(format!(
                "Transaction {} is not in Validated state", id
            ))),
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

    /// Acquire a mutable entry for state transitions.
    /// Returns `Err` if the transaction doesn't exist.
    pub fn get_transaction_mut(&self, id: &str) -> Result<dashmap::mapref::one::RefMut<String, PendingTransaction>, PneumaticError> {
        self.transactions.get_mut(id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not found in registry", id
            )))
    }

    /// Check if any transaction is awaiting finalizer assignment.
    pub fn transaction_is_awaiting_finalizer(&self, id: &str) -> bool {
        let entry = match self.transactions.get_mut(id) {
            Some(e) => e,
            None => return false,
        };
        entry.is_awaiting_finalizer()
    }

    /// Enqueue a transaction into the pool. Called when a transaction
    /// enters the Validated state.
    ///
    /// Rejects a replayed nonce (Phase 5.6 / H14): the same
    /// `(token_id, sender, sequence_number)` may never be admitted twice. The
    /// insertion point, so this also keeps the pool itself free of duplicate
    /// `(sender, seq)` entries.
    pub fn enqueue_to_pool(&self, tx_id: &str, token_id: Vec<u8>,
                           sequence_number: usize, timestamp: i64, sender: Vec<u8>)
                           -> Result<(), PneumaticError> {
        // Guard the nonce before touching the pool so a duplicate is rejected at admission.
        let key = (token_id.clone(), sender.clone(), sequence_number);
        if self.used_nonces.insert(key, ()).is_some() {
            return Err(PneumaticError::Registry(format!(
                "duplicate nonce: (token_id={:?}, sender={:?}, sequence_number={}) already admitted",
                token_id, sender, sequence_number
            )));
        }
        let mut pool = self.pool.lock().unwrap();
        pool.enqueue(tx_id.to_string(), token_id, sequence_number, timestamp, sender);
        Ok(())
    }

    /// Dequeue the top n transaction IDs for a token. Returns IDs in
    /// deterministic order for leader block proposal.
    pub fn dequeue_for_leader(&self, token_id: &[u8], n: usize) -> Vec<String> {
        let mut pool = self.pool.lock().unwrap();
        pool.dequeue_for_leader(token_id, n)
    }

    /// Remove a transaction from the pool (called on commit or failure).
    pub fn remove_from_pool(&self, tx_id: &str) {
        let mut pool = self.pool.lock().unwrap();
        pool.remove(tx_id);
    }

    /// Get an immutable clone of a transaction from the Validated state.
    pub fn get_transaction(&self, id: &str) -> Result<Transaction, PneumaticError> {
        let entry = self.transactions.get(id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not found in registry", id
            )))?;
        match &entry.state {
            TransactionState::Validated { transaction, .. } => Ok(transaction.clone()),
            _ => Err(PneumaticError::Registry(format!(
                "Transaction {} is not in Validated state", id
            ))),
        }
    }

    /// Get ordered transactions for a token: dequeues from pool, fetches
    /// each from the registry, returns Vec of Transactions in deterministic order.
    pub fn get_ordered_transactions(&self, token_id: &[u8], limit: usize)
        -> Result<Vec<Transaction>, PneumaticError>
    {
        let tx_ids = self.dequeue_for_leader(token_id, limit);
        let mut result = Vec::with_capacity(tx_ids.len());
        for tx_id in tx_ids {
            let tx = self.get_transaction(&tx_id)?;
            result.push(tx);
        }
        Ok(result)
    }

    /// Record the gas used for a transaction, computed during validation.
    pub fn record_gas_used(&self, tx_id: &str, gas_used: u64) {
        self.gas_tracker.lock().unwrap().insert(tx_id.to_string(), gas_used);
    }

    /// Retrieve the gas used for a transaction. Returns None if not tracked.
    pub fn get_gas_used(&self, tx_id: &str) -> Option<u64> {
        self.gas_tracker.lock().unwrap().get(tx_id).copied()
    }

    /// Record an admin tax credit collected during token minting.
    /// Returns the credit ID for later redemption.
    pub fn record_admin_credit(&self, credit: PendingAdminCredit) {
        self.admin_credits.insert(credit.id.clone(), credit);
    }

    /// Retrieve a pending admin credit by ID.
    pub fn get_admin_credit(&self, credit_id: &str) -> Option<PendingAdminCredit> {
        self.admin_credits.get(credit_id).map(|c| c.clone())
    }

    /// Take (remove and return) a pending admin credit — used for redemption.
    pub fn take_admin_credit(&self, credit_id: &str) -> Option<PendingAdminCredit> {
        let entry = self.admin_credits.remove(credit_id)?;
        Some(entry.1)
    }

    /// Transition to Validated state and enqueue into the pool in one
    /// atomic operation. The pool insertion uses the transaction's own
    /// token_id, sequence_number, timestamp, and sender.
    pub fn transition_to_validated_and_enqueue(
        &self,
        tx_id: &str,
        transaction: Transaction,
        validation: TransactionValidationResult,
    ) -> Result<(), PneumaticError> {
        // Transition the state
        {
            let mut entry = self.transactions.get_mut(tx_id)
                .ok_or_else(|| PneumaticError::Registry(format!(
                    "Transaction {} not found", tx_id
                )))?;
            entry.transition_to_validated(transaction.clone(), validation);
        }
        // Enqueue into the pool (rejects a replayed nonce — Phase 5.6 / H14)
        self.enqueue_to_pool(
            tx_id,
            transaction.token_id.clone(),
            transaction.sequence_number,
            transaction.timestamp,
            transaction.sender.clone(),
        )?;
        Ok(())
    }
}
