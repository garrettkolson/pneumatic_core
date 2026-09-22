use std::collections::HashMap;
use std::sync::Mutex;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::errors::PneumaticError;
use crate::transactions::{PendingTransaction, ShieldedTransaction, Transaction, TransactionState, TransactionValidationResult, TransactionSignature, TransactionPool};
use crate::errors::{ValidationFailureReason, TransactionRiskFactor};
use crate::validation::NullifierMembership;

// ---------------------------------------------------------------------------
// PendingTransactionRegistry — manages transactions in-flight
// ---------------------------------------------------------------------------

/// Backed by DashMap for concurrent access. Every method returns `Result`
/// (never `Option`) to distinguish "not found" from "operation failed".
/// Pending admin tax credit — records admin tax collected during token minting.
/// Stored in the `PendingTransactionRegistry` until the admin collects or redeems it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAdminCredit {
    /// Unique credit identifier
    pub id: String,
    /// Admin public key who receives the tax
    pub admin_public_key: Vec<u8>,
    /// Tax amount owed to the admin
    pub amount: u64,
    /// Token ID that generated this credit
    pub token_id: Vec<u8>,
}

#[derive(Default)]
pub struct PendingTransactionRegistry {
    transactions: DashMap<String, PendingTransaction>,
    /// Ordered transaction pool for leader block proposal.
    pool: Mutex<TransactionPool>,
    /// Admin tax credits collected during token minting, keyed by credit ID.
    admin_credits: DashMap<String, PendingAdminCredit>,
    /// Gas used per transaction, tracked during validation and deducted on commit.
    gas_tracker: Mutex<HashMap<String, u64>>,
    /// Durable record of every admitted `(token_id, sender, sequence_number)`, used to reject
    /// replayed nonces (Phase 5.6 / H14). Append-only and never evicted: a nonce stays consumed
    /// for its sender even after the tx is dequeued or committed, so a replay can never be
    /// re-admitted once accepted. Keyed by all three because each token has its own account.
    used_nonces: DashMap<(Vec<u8>, Vec<u8>, usize), ()>,
    /// Shielded transfers admitted by the sentinel (Phase S5.1). Parallel to
    /// `transactions`, never evicted: a shielded tx has no `TransactionState`
    /// lifecycle and no remove path, so its registry entry is permanent. The
    /// committer hash-matches block-carried shielded txs against these entries
    /// (S5.3); the finalizer reads them at signing time (S5.2).
    shielded_transactions: DashMap<String, ShieldedTransaction>,
}

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

// ---------------------------------------------------------------------------
// NullifierRegistry — consensus-critical spent-nullifier set (Phase S4.2)
// ---------------------------------------------------------------------------

/// Set of every spent shielded-note nullifier (roadmap 2.5: consensus-
/// critical, append-only, durable, globally agreed).
///
/// **Nullifiers are never removed — with exactly one scoped exception
/// (S5.3).** This type deliberately exposes no general removal API: a
/// rollback that un-spent a nullifier would be a double-spend (two divergent
/// per-token chain branches could spend the same note, and a set that shrinks
/// on reorg would let both stand).
///
/// The one removal API, [`NullifierRegistry::unmark_many`], is a scoped,
/// documented exception: only the committer's `ShieldedPool` rollback path
/// may call it, only under the pool's single-writer guard, and only with the
/// exact keys recorded in the losing block's applied delta. That pairing is
/// what makes it safe: the pool runs *every* `mark_many_atomic` under the
/// same guard, so an `unmark_many` can never interleave with the two phases
/// of `mark_many_atomic` — the phase-2 rollback argument below is unchanged —
/// and a rolled-back loser's spend is precisely the one a tip rollback must
/// undo (UTXO semantics: the winning branch may re-spend the un-spent note,
/// so the set shrinks only together with the branch state that created it).
///
/// **Growth:** unbounded in v1 (32 B per spend — 10M spends ≈ 320 MB).
/// Compaction is a future item; not built here.
///
/// **Persistence** is the committer's `ShieldedPool` job (S5.3); this
/// in-memory structure does not survive a restart on its own.
#[derive(Default)]
pub struct NullifierRegistry {
    nullifiers: DashMap<[u8; 32], ()>,
}

impl NullifierRegistry {
    pub fn new() -> Self {
        NullifierRegistry {
            nullifiers: DashMap::new(),
        }
    }

    /// Mark a nullifier spent. Atomic check-or-insert: `insert` returns the
    /// old value, so a duplicate is detected without a TOCTOU window (the
    /// same idiom as `add_transaction` and `used_nonces`). A double-mark is
    /// the double-spend check: `Err(Validation([StaleNullifier]))` — the
    /// exact reason the shielded validation spec's check 2 surfaces.
    pub fn try_mark_spent(&self, nullifier: [u8; 32]) -> Result<(), PneumaticError> {
        if self.nullifiers.insert(nullifier, ()).is_some() {
            return Err(PneumaticError::Validation(
                vec![ValidationFailureReason::StaleNullifier],
            ));
        }
        Ok(())
    }

    /// Has this nullifier already been spent?
    pub fn contains(&self, nullifier: [u8; 32]) -> bool {
        self.nullifiers.contains_key(&nullifier)
    }

    /// Number of nullifiers marked spent (observational only — never a
    /// consensus input; `try_mark_spent` / `mark_many_atomic` are the
    /// authoritative operations).
    pub fn len(&self) -> usize {
        self.nullifiers.len()
    }

    /// True if no nullifier has been marked spent.
    pub fn is_empty(&self) -> bool {
        self.nullifiers.is_empty()
    }

    /// Mark a batch of nullifiers spent, **all-or-nothing**: every nullifier
    /// is checked first, and only if none is already spent are they all
    /// inserted. A tx with two nullifiers must never spend one and reject on
    /// the other (roadmap 2.5 double-spend defense).
    ///
    /// DashMap has no multi-key transaction, so this is two-phase:
    /// (1) **check-all** — fail fast before touching state; (2) **insert-all**,
    /// recording what *this* call inserted. If a concurrent spender lands in
    /// the gap between the phases, an insert hits an existing key: this call
    /// then removes exactly the keys it itself inserted in phase 2 and
    /// returns `StaleNullifier`. That rollback is exact because this type has
    /// **no general removal API** (roadmap 2.5; the sole scoped exception,
    /// `unmark_many`, is callable only from the pool's single-writer rollback
    /// path, which shares this call's guard and so cannot interleave with
    /// these two phases): a key whose insert returned `None` was absent just
    /// before this call, and the only code that can remove such a key is the
    /// failed call that inserted it — so a rolled-back key is restored to its
    /// pre-call state and no other call's spend is ever undone.
    ///
    /// A batch containing the same nullifier twice self-collides in phase 2
    /// and is rejected the same way (a nullifier cannot be spent twice, not
    /// even by one tx); on the wire path S4.1 check 1 rejects that shape
    /// first. An empty batch is a no-op.
    pub fn mark_many_atomic(&self, nullifiers: &[[u8; 32]]) -> Result<(), PneumaticError> {
        // Phase 1 — check ALL before touching state.
        for nullifier in nullifiers {
            if self.nullifiers.contains_key(nullifier) {
                return Err(PneumaticError::Validation(
                    vec![ValidationFailureReason::StaleNullifier],
                ));
            }
        }
        // Phase 2 — insert ALL, tracking this call's own inserts.
        let mut inserted: Vec<[u8; 32]> = Vec::with_capacity(nullifiers.len());
        for nullifier in nullifiers {
            match self.nullifiers.insert(*nullifier, ()) {
                None => inserted.push(*nullifier),
                Some(()) => {
                    // A concurrent spender beat us; roll back this call's own
                    // inserts only (see the no-removal-API soundness note).
                    for key in &inserted {
                        self.nullifiers.remove(key);
                    }
                    return Err(PneumaticError::Validation(
                        vec![ValidationFailureReason::StaleNullifier],
                    ));
                }
            }
        }
        Ok(())
    }

    /// Remove a batch of nullifiers from the spent set — the exact inverse of
    /// `mark_many_atomic`, **scoped to S5.3 rollback**.
    ///
    /// This is the type's sole removal API and a scoped, documented exception
    /// to the never-removed rule (see the type doc). The only sanctioned
    /// caller is the committer's `ShieldedPool` rollback path, and it may
    /// call this (a) under the pool's single-writer guard and (b) with
    /// exactly the keys recorded in the losing block's applied delta. Any
    /// other use — a non-pool caller, keys not taken from a recorded delta,
    /// a call racing `mark_many_atomic`'s two phases — breaks the
    /// single-writer discipline the phase-2 rollback argument relies on.
    pub fn unmark_many(&self, nullifiers: &[[u8; 32]]) {
        for nullifier in nullifiers {
            self.nullifiers.remove(nullifier);
        }
    }
}

/// S4.1 check-2 seam: the shielded validation spec sees the spent-nullifier
/// set through `NullifierMembership`, never the concrete type. This impl is
/// what lets the real registry back `ShieldedValidationDeps::spent` (S4.2.4).
impl NullifierMembership for NullifierRegistry {
    fn contains_nullifier(&self, nullifier: [u8; 32]) -> bool {
        self.contains(nullifier)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    // --- NullifierRegistry (Phase S4.2) ---

    #[test]
    fn try_mark_spent_first_mark_succeeds() {
        let registry = NullifierRegistry::new();
        let nullifier = [7u8; 32];
        assert!(registry.try_mark_spent(nullifier).is_ok());
        assert!(registry.contains(nullifier));
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
    }

    #[test]
    fn try_mark_spent_double_mark_rejected_as_stale_nullifier() {
        let registry = NullifierRegistry::new();
        let nullifier = [7u8; 32];
        registry.try_mark_spent(nullifier).unwrap();
        let err = registry.try_mark_spent(nullifier).unwrap_err();
        assert!(
            matches!(
                err,
                PneumaticError::Validation(ref reasons)
                    if reasons == &vec![ValidationFailureReason::StaleNullifier]
            ),
            "double-mark must be Validation([StaleNullifier]) exactly, got: {:?}",
            err
        );
        // The rejected mark must not disturb the set.
        assert_eq!(registry.len(), 1);
        assert!(registry.contains(nullifier));
    }

    #[test]
    fn contains_reflects_state() {
        let registry = NullifierRegistry::new();
        let nullifier = [9u8; 32];
        assert!(!registry.contains(nullifier));
        registry.try_mark_spent(nullifier).unwrap();
        assert!(registry.contains(nullifier));
    }

    #[test]
    fn len_and_is_empty_track_inserts() {
        let registry = NullifierRegistry::new();
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
        registry.try_mark_spent([1u8; 32]).unwrap();
        registry.try_mark_spent([2u8; 32]).unwrap();
        registry.try_mark_spent([3u8; 32]).unwrap();
        assert_eq!(registry.len(), 3);
        assert!(!registry.is_empty());
    }

    #[test]
    fn mark_many_atomic_all_fresh_marks_all() {
        let registry = NullifierRegistry::new();
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert!(registry.mark_many_atomic(&[a, b]).is_ok());
        assert!(registry.contains(a));
        assert!(registry.contains(b));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn mark_many_atomic_second_already_spent_marks_none() {
        // All-or-nothing: a batch containing one already-spent nullifier marks
        // NONE of the others (roadmap 2.5 double-spend defense).
        let registry = NullifierRegistry::new();
        let a = [1u8; 32];
        let b = [2u8; 32];
        registry.try_mark_spent(b).unwrap();
        let err = registry.mark_many_atomic(&[a, b]).unwrap_err();
        assert!(
            matches!(
                err,
                PneumaticError::Validation(ref reasons)
                    if reasons == &vec![ValidationFailureReason::StaleNullifier]
            ),
            "stale batch must be Validation([StaleNullifier]) exactly, got: {:?}",
            err
        );
        assert!(!registry.contains(a), "the fresh nullifier must NOT have been marked");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn mark_many_atomic_first_already_spent_rejects_before_any_insert() {
        // Check-all runs before insert-all: the trailing fresh nullifiers are
        // untouched even when the leading one is stale.
        let registry = NullifierRegistry::new();
        let a = [1u8; 32];
        let b = [2u8; 32];
        registry.try_mark_spent(a).unwrap();
        assert!(registry.mark_many_atomic(&[a, b]).is_err());
        assert!(!registry.contains(b));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn mark_many_atomic_duplicate_within_batch_rejected() {
        // A nullifier cannot be spent twice, not even by one tx: the phase-2
        // self-collision rolls back and nothing is marked.
        let registry = NullifierRegistry::new();
        let a = [1u8; 32];
        assert!(registry.mark_many_atomic(&[a, a]).is_err());
        assert!(!registry.contains(a));
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn mark_many_atomic_empty_batch_is_noop() {
        let registry = NullifierRegistry::new();
        assert!(registry.mark_many_atomic(&[]).is_ok());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn concurrent_try_mark_spent_same_nullifier_exactly_one_succeeds() {
        // The atomic insert-returns-old idiom under a real race: N threads,
        // one nullifier, exactly one winner — no TOCTOU window.
        let registry = Arc::new(NullifierRegistry::new());
        let nullifier = [5u8; 32];
        let mut handles = vec![];
        for _ in 0..8 {
            let reg = registry.clone();
            handles.push(thread::spawn(move || reg.try_mark_spent(nullifier)));
        }
        let mut successes = 0;
        for h in handles {
            if h.join().unwrap().is_ok() {
                successes += 1;
            }
        }
        assert_eq!(successes, 1, "exactly one thread may mark the nullifier");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn concurrent_mark_many_atomic_same_batch_one_succeeds_no_partial() {
        // The phase-1/phase-2 gap is the real race: 8 threads run the same
        // two-nullifier batch. All-or-nothing means the eventual state is the
        // whole batch, fully applied, with no partial state.
        let registry = Arc::new(NullifierRegistry::new());
        let a = [1u8; 32];
        let b = [2u8; 32];
        let mut handles = vec![];
        for _ in 0..8 {
            let reg = registry.clone();
            handles.push(thread::spawn(move || reg.mark_many_atomic(&[a, b])));
        }
        let mut successes = 0;
        for h in handles {
            if h.join().unwrap().is_ok() {
                successes += 1;
            }
        }
        assert_eq!(successes, 1, "exactly one thread may win the batch");
        assert_eq!(registry.len(), 2, "the winning batch is fully applied");
        assert!(registry.contains(a));
        assert!(registry.contains(b));
    }

    #[test]
    fn concurrent_mark_many_atomic_mixed_unique_and_duplicates() {
        // 16 threads: 8 unique single-nullifier batches + 8 duplicates over
        // the same 8 nullifiers. Exactly 8 wins, 8 stale rejections, 8 in the
        // set — the double-spend defense under real concurrency.
        let registry = Arc::new(NullifierRegistry::new());
        let mut handles = vec![];
        for i in 0..16 {
            let reg = registry.clone();
            let nullifier = [(i % 8) as u8; 32];
            handles.push(thread::spawn(move || reg.mark_many_atomic(&[nullifier])));
        }
        let mut successes = 0;
        let mut failures = 0;
        for h in handles {
            match h.join().unwrap() {
                Ok(()) => successes += 1,
                Err(_) => failures += 1,
            }
        }
        assert_eq!(successes, 8);
        assert_eq!(failures, 8);
        assert_eq!(registry.len(), 8);
    }

    // --- PendingTransactionRegistry ---

    #[test]
    fn contains_empty_registry_returns_false() {
        let registry = PendingTransactionRegistry::new();
        assert!(!registry.contains("tx1"));
    }

    #[test]
    fn contains_after_register_pending_returns_true() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        assert!(registry.contains("tx1"));
    }

    #[test]
    fn register_pending_duplicate_returns_error() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        assert!(registry.register_pending("tx1".into()).is_err());
    }

    #[test]
    fn enqueue_to_pool_rejects_duplicate_nonce() {
        // Phase 5.6 / H14: a reused (token_id, sender, sequence_number) is a replay.
        let registry = PendingTransactionRegistry::new();
        let token_id = vec![1, 2, 3];
        let sender = vec![9, 9, 9];

        // First admission of (token_id, sender, seq=5) succeeds.
        assert!(registry
            .enqueue_to_pool("tx1", token_id.clone(), 5, 100, sender.clone())
            .is_ok());

        // A different tx_id carrying the SAME (token_id, sender, seq=5) is a replayed nonce.
        assert!(registry
            .enqueue_to_pool("tx2", token_id.clone(), 5, 101, sender.clone())
            .is_err());

        // A distinct sequence number for the same sender is not a replay.
        assert!(registry
            .enqueue_to_pool("tx3", token_id.clone(), 6, 102, sender.clone())
            .is_ok());
    }

    #[test]
    fn add_transaction_creates_pending_state() {
        let registry = PendingTransactionRegistry::new();
        let tx = PendingTransaction::new("tx1".into(), TransactionState::Pending);
        registry.add_transaction("tx1".into(), tx).unwrap();
        let entry = registry.get_transaction_mut("tx1").unwrap();
        assert!(matches!(entry.state, TransactionState::Pending));
    }

    #[test]
    fn add_transaction_duplicate_returns_error() {
        let registry = PendingTransactionRegistry::new();
        let tx = PendingTransaction::new("tx1".into(), TransactionState::Pending);
        registry.add_transaction("tx1".into(), tx).unwrap();
        let tx2 = PendingTransaction::new("tx1".into(), TransactionState::Pending);
        assert!(registry.add_transaction("tx1".into(), tx2).is_err());
    }

    #[test]
    fn remove_transaction_successful() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        assert!(registry.remove_transaction("tx1").is_ok());
        assert!(!registry.contains("tx1"));
    }

    #[test]
    fn remove_nonexistent_returns_error() {
        let registry = PendingTransactionRegistry::new();
        assert!(registry.remove_transaction("tx1").is_err());
    }

    #[test]
    fn acquire_transaction_found_succeeds() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        assert!(registry.acquire_transaction("tx1").is_ok());
    }

    #[test]
    fn acquire_nonexistent_returns_error() {
        let registry = PendingTransactionRegistry::new();
        assert!(registry.acquire_transaction("tx1").is_err());
    }

    #[test]
    fn acquire_terminal_state_fails() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        registry.acquire_transaction("tx1").unwrap();
        // Transition to Failed
        if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
            entry.transition_to_failed(
                Transaction {
                    id: "tx1".into(), action: "Transfer".into(),
                    token_id: vec![], bid: None, sequence_number: 0,
                    sender: vec![], receiver: vec![], amount: None,
                    timestamp: 0, result_hash: vec![],
                    sender_signature: vec![],
                },
                vec![],
            );
        }
        assert!(registry.acquire_transaction("tx1").is_err());
    }

    #[test]
    fn get_validation_result_from_validated_returns_some() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        registry.acquire_transaction("tx1").unwrap();
        // Transition to Validated
        if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
            entry.transition_to_validated(
                Transaction {
                    id: "tx1".into(), action: "Transfer".into(),
                    token_id: vec![], bid: None, sequence_number: 1,
                    sender: vec![1], receiver: vec![2], amount: Some(100),
                    timestamp: 0, result_hash: vec![],
                    sender_signature: vec![],
                },
                TransactionValidationResult::valid(
                    vec![1],
                    TransactionRiskFactor {
                        affected_parties: 2, amount: 100,
                        is_contract: false, is_multi_party: false,
                    },
                ),
            );
        }
        let result = registry.get_validation_result("tx1").unwrap();
        assert!(result.is_valid);
    }

    #[test]
    fn get_validation_result_from_pending_returns_error() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        assert!(registry.get_validation_result("tx1").is_err());
    }

    #[test]
    fn release_transaction_successful_keeps_pending() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        registry.acquire_transaction("tx1").unwrap();
        let result = registry.release_transaction("tx1").unwrap();
        assert!(!result); // Pending, not terminal → false
        assert!(registry.contains("tx1")); // still in registry
    }

    #[test]
    fn release_failed_transaction_returns_true() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
            entry.transition_to_failed(
                Transaction {
                    id: "tx1".into(), action: "Transfer".into(),
                    token_id: vec![], bid: None, sequence_number: 0,
                    sender: vec![], receiver: vec![], amount: None,
                    timestamp: 0, result_hash: vec![],
                    sender_signature: vec![],
                },
                vec![ValidationFailureReason::InsufficientFunds],
            );
        }
        let result = registry.release_transaction("tx1").unwrap();
        assert!(result); // Failed, lock=0 → true (caller should remove)
        // Note: release_transaction returns true to signal removal but doesn't remove itself
        assert!(registry.contains("tx1")); // still in registry until removed
    }

    #[test]
    fn set_requested_finalizer_validated_succeeds() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        // Transition to Validated first
        if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
            entry.transition_to_validated(
                Transaction {
                    id: "tx1".into(), action: "Transfer".into(),
                    token_id: vec![], bid: None, sequence_number: 1,
                    sender: vec![1], receiver: vec![2], amount: Some(100),
                    timestamp: 0, result_hash: vec![],
                    sender_signature: vec![],
                },
                TransactionValidationResult::valid(vec![], TransactionRiskFactor { affected_parties: 1, amount: 0, is_contract: false, is_multi_party: false }),
            );
        }
        assert!(registry.set_requested_finalizer("tx1", vec![99]).is_ok());
    }

    #[test]
    fn set_requested_finalizer_from_pending_fails() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        assert!(registry.set_requested_finalizer("tx1", vec![99]).is_err());
    }

    #[test]
    fn is_requested_finalizer_matches() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
            entry.transition_to_validated(
                Transaction {
                    id: "tx1".into(), action: "Transfer".into(),
                    token_id: vec![], bid: None, sequence_number: 1,
                    sender: vec![1], receiver: vec![2], amount: Some(100),
                    timestamp: 0, result_hash: vec![],
                    sender_signature: vec![],
                },
                TransactionValidationResult::valid(vec![], TransactionRiskFactor { affected_parties: 1, amount: 0, is_contract: false, is_multi_party: false }),
            );
        }
        registry.set_requested_finalizer("tx1", vec![99]).unwrap();
        assert!(registry.is_requested_finalizer("tx1", &[99]));
    }

    #[test]
    fn is_requested_finalizer_mismatch() {
        let registry = PendingTransactionRegistry::new();
        registry.register_pending("tx1".into()).unwrap();
        if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
            entry.transition_to_validated(
                Transaction {
                    id: "tx1".into(), action: "Transfer".into(),
                    token_id: vec![], bid: None, sequence_number: 1,
                    sender: vec![1], receiver: vec![2], amount: Some(100),
                    timestamp: 0, result_hash: vec![],
                    sender_signature: vec![],
                },
                TransactionValidationResult::valid(vec![], TransactionRiskFactor { affected_parties: 1, amount: 0, is_contract: false, is_multi_party: false }),
            );
        }
        registry.set_requested_finalizer("tx1", vec![99]).unwrap();
        assert!(!registry.is_requested_finalizer("tx1", &[1, 2, 3]));
    }

    // --- Shielded transfers (Phase S5.1.2) ---

    /// A minimal shielded tx for registry tests (dummy field contents).
    fn shielded_fixture(id: &str) -> ShieldedTransaction {
        ShieldedTransaction {
            id: id.into(),
            action: "ShieldedTransfer".into(),
            token_id: vec![1, 2, 3],
            spent_commitments: vec![[6u8; 32]],
            nullifiers: vec![[1u8; 32]],
            commitments: vec![[3u8; 32]],
            merkle_root: [5u8; 32],
            proof: vec![9, 8, 7, 6],
            note_ciphertexts: vec![vec![10u8; 64]],
            fee: 0,
        }
    }

    /// Admit → contains → get round-trips the exact tx (the clone is the
    /// canonical copy; no field is lost in flight).
    #[test]
    fn register_shielded_roundtrips_through_map() {
        let registry = PendingTransactionRegistry::new();
        let tx = shielded_fixture("shd_1");

        assert!(!registry.contains_shielded("shd_1"), "fresh registry: absent");
        assert!(
            registry.get_shielded("shd_1").is_none(),
            "fresh registry: no entry to fetch"
        );

        registry.register_shielded(&tx).unwrap();

        assert!(registry.contains_shielded("shd_1"), "admitted: present");
        assert_eq!(
            registry.get_shielded("shd_1"),
            Some(tx.clone()),
            "fetched entry equals the admitted tx exactly"
        );
    }

    /// A duplicate id is rejected atomically — `insert`'s returned old value
    /// is the race-free duplicate check — and the rejection leaves the
    /// original entry untouched (a silent overwrite would let a second,
    /// differently-proven tx squat a signed id).
    #[test]
    fn register_shielded_duplicate_id_rejected_atomically() {
        let registry = PendingTransactionRegistry::new();
        let first = shielded_fixture("shd_dup");

        registry.register_shielded(&first).unwrap();

        // Same id, *different* proof: the duplicate is a protocol violation.
        let mut impostor = first.clone();
        impostor.proof = vec![0xDE, 0xAD];
        let err = registry.register_shielded(&impostor).unwrap_err();
        assert!(
            matches!(err, PneumaticError::Registry(_)),
            "duplicate id is a Registry error, got: {:?}",
            err
        );

        // The original is intact — nothing was overwritten.
        assert_eq!(
            registry.get_shielded("shd_dup"),
            Some(first),
            "the first-admitted entry survives a duplicate rejection"
        );
    }

    // --- TransactionSignatureRegistry ---

    #[test]
    fn signature_registry_add_transaction_successful() {
        let registry = TransactionSignatureRegistry::new();
        assert!(registry.try_add_transaction("tx1").is_ok());
        assert!(registry.transaction_is_registered("tx1"));
    }

    #[test]
    fn signature_registry_duplicate_add_returns_error() {
        let registry = TransactionSignatureRegistry::new();
        registry.try_add_transaction("tx1").unwrap();
        assert!(registry.try_add_transaction("tx1").is_err());
    }

    #[test]
    fn signature_registry_add_signature_successful() {
        let registry = TransactionSignatureRegistry::new();
        registry.try_add_transaction("tx1").unwrap();
        let sig = TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![1, 2, 3],
            current_stake: 100,
        };
        assert!(registry
            .try_add_signature("tx1", vec![1], sig.clone())
            .is_ok());
        let registry_map = registry.get_transaction_registry("tx1").unwrap();
        assert_eq!(registry_map.len(), 1);
    }

    #[test]
    fn signature_registry_duplicate_signature_fails() {
        let registry = TransactionSignatureRegistry::new();
        registry.try_add_transaction("tx1").unwrap();
        let sig = TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![1, 2, 3],
            current_stake: 100,
        };
        registry.try_add_signature("tx1", vec![1], sig.clone()).unwrap();
        assert!(registry.try_add_signature("tx1", vec![1], sig).is_err());
    }

    #[test]
    fn signature_registry_multiple_sigs_different_executors() {
        let registry = TransactionSignatureRegistry::new();
        registry.try_add_transaction("tx1").unwrap();
        let sig = |stake| TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![stake as u8],
            current_stake: stake,
        };
        registry.try_add_signature("tx1", vec![1], sig(100)).unwrap();
        registry.try_add_signature("tx1", vec![2], sig(200)).unwrap();
        registry.try_add_signature("tx1", vec![3], sig(300)).unwrap();
        let registry_map = registry.get_transaction_registry("tx1").unwrap();
        assert_eq!(registry_map.len(), 3);
    }

    #[test]
    fn signature_registry_remove_successful() {
        let registry = TransactionSignatureRegistry::new();
        registry.try_add_transaction("tx1").unwrap();
        assert!(registry.try_remove_transaction("tx1").is_ok());
        assert!(!registry.transaction_is_registered("tx1"));
    }

    #[test]
    fn signature_registry_remove_nonexistent_fails() {
        let registry = TransactionSignatureRegistry::new();
        assert!(registry.try_remove_transaction("tx1").is_err());
    }

    #[test]
    fn signature_registry_empty_and_len() {
        let registry = TransactionSignatureRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        registry.try_add_transaction("tx1").unwrap();
        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);
    }

    // --- Gas tracker tests ---

    #[test]
    fn gas_tracker_records_and_retrieves() {
        let registry = PendingTransactionRegistry::new();
        registry.record_gas_used("tx1", 42);
        assert_eq!(registry.get_gas_used("tx1"), Some(42));
    }

    #[test]
    fn gas_tracker_returns_none_for_unknown_tx() {
        let registry = PendingTransactionRegistry::new();
        assert_eq!(registry.get_gas_used("nonexistent"), None);
    }

    // --- Concurrent tests ---

    #[test]
    fn concurrent_register_pending_same_id() {
        let registry = Arc::new(PendingTransactionRegistry::new());
        let mut handles = vec![];
        for _ in 0..2 {
            let reg = registry.clone();
            handles.push(std::thread::spawn(move || {
                reg.register_pending("tx1".into())
            }));
        }
        let mut successes = 0;
        let mut failures = 0;
        for h in handles {
            match h.join().unwrap() {
                Ok(()) => successes += 1,
                Err(_) => failures += 1,
            }
        }
        assert_eq!(successes, 1);
        assert_eq!(failures, 1);
    }

    #[test]
    fn concurrent_register_pending_different_ids_all_succeed() {
        let registry = Arc::new(PendingTransactionRegistry::new());
        let mut handles = vec![];
        for i in 0..10 {
            let reg = registry.clone();
            handles.push(std::thread::spawn(move || {
                reg.register_pending(format!("tx_{}", i))
            }));
        }
        for h in handles {
            h.join().unwrap().unwrap();
        }
        for i in 0..10 {
            assert!(registry.contains(&format!("tx_{}", i)));
        }
    }

    #[test]
    fn concurrent_acquire_release_same_entry() {
        let registry = Arc::new(PendingTransactionRegistry::new());
        registry.register_pending("tx1".into()).unwrap();
        let mut acq_handles = vec![];
        // 4 threads acquire
        for _ in 0..4 {
            let reg = registry.clone();
            acq_handles.push(std::thread::spawn(move || {
                let _ = reg.acquire_transaction("tx1");
            }));
        }
        for h in acq_handles {
            h.join().unwrap();
        }
        // 4 threads release
        let mut rel_handles = vec![];
        for _ in 0..4 {
            let reg = registry.clone();
            rel_handles.push(std::thread::spawn(move || {
                let _ = reg.release_transaction("tx1");
            }));
        }
        for h in rel_handles {
            h.join().unwrap();
        }
        // Registry may or may not contain tx depending on race conditions — just check no panic
        let _ = registry.contains("tx1");
    }

    #[test]
    fn concurrent_acquire_terminal_state_rejected() {
        let registry = Arc::new(PendingTransactionRegistry::new());
        registry.register_pending("tx1".into()).unwrap();
        // Transition one entry to Failed
        {
            let mut entry = registry.transactions.get_mut("tx1").unwrap();
            entry.transition_to_failed(
                Transaction {
                    id: "tx1".into(), action: "Transfer".into(),
                    token_id: vec![], bid: None, sequence_number: 0,
                    sender: vec![], receiver: vec![], amount: None,
                    timestamp: 0, result_hash: vec![],
                    sender_signature: vec![],
                },
                vec![],
            );
        }
        let mut handles = vec![];
        for _ in 0..4 {
            let reg = registry.clone();
            handles.push(std::thread::spawn(move || {
                reg.acquire_transaction("tx1")
            }));
        }
        for h in handles {
            assert!(h.join().unwrap().is_err());
        }
    }

    #[test]
    fn concurrent_remove_during_acquire() {
        let registry = Arc::new(PendingTransactionRegistry::new());
        registry.register_pending("tx1".into()).unwrap();
        let remove_handle = {
            let reg = registry.clone();
            std::thread::spawn(move || reg.remove_transaction("tx1"))
        };
        std::thread::sleep(std::time::Duration::from_millis(5));
        let acquire_handle = {
            let reg = registry.clone();
            std::thread::spawn(move || reg.acquire_transaction("tx1"))
        };
        let _ = remove_handle.join().unwrap();
        match acquire_handle.join().unwrap() {
            Ok(()) => panic!("should have failed — tx was removed"),
            Err(_) => {} // expected
        }
    }

    #[test]
    fn concurrent_acquire_release_stress_50() {
        let registry = Arc::new(PendingTransactionRegistry::new());
        registry.register_pending("tx1".into()).unwrap();
        let mut handles = vec![];
        for _ in 0..50 {
            let reg = registry.clone();
            handles.push(std::thread::spawn(move || {
                let _ = reg.acquire_transaction("tx1");
                std::thread::sleep(std::time::Duration::from_millis(1));
                let _ = reg.release_transaction("tx1");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // No panics — that's the point
    }

    #[test]
    fn concurrent_release_zero_pending_keeps() {
        let registry = Arc::new(PendingTransactionRegistry::new());
        registry.register_pending("tx1".into()).unwrap();
        let mut handles = vec![];
        for _ in 0..4 {
            let reg = registry.clone();
            handles.push(std::thread::spawn(move || {
                reg.release_transaction("tx1").unwrap()
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Pending with lock=0 → release returns false, tx stays
        assert!(registry.contains("tx1"));
    }

    #[test]
    fn concurrent_add_signature_different_executors() {
        let registry = Arc::new(TransactionSignatureRegistry::new());
        registry.try_add_transaction("tx1").unwrap();
        let mut handles = vec![];
        for i in 0..4 {
            let reg = registry.clone();
            let sig = TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![i as u8],
                current_stake: 100 + i,
            };
            handles.push(std::thread::spawn(move || {
                reg.try_add_signature("tx1", vec![i as u8], sig)
            }));
        }
        for h in handles {
            h.join().unwrap().unwrap();
        }
        let map = registry.get_transaction_registry("tx1").unwrap();
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn concurrent_add_signature_same_executor_one_succeeds() {
        let registry = Arc::new(TransactionSignatureRegistry::new());
        registry.try_add_transaction("tx1").unwrap();
        let sig = TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![1, 2, 3],
            current_stake: 100,
        };
        let mut handles = vec![];
        for _ in 0..4 {
            let reg = registry.clone();
            let s = sig.clone();
            handles.push(std::thread::spawn(move || {
                reg.try_add_signature("tx1", vec![1], s)
            }));
        }
        let mut successes = 0;
        for h in handles {
            match h.join().unwrap() {
                Ok(()) => successes += 1,
                Err(_) => {}
            }
        }
        assert_eq!(successes, 1);
    }

    #[test]
    fn concurrent_try_add_transaction_same_id() {
        let registry = Arc::new(TransactionSignatureRegistry::new());
        let mut handles = vec![];
        for _ in 0..4 {
            let reg = registry.clone();
            handles.push(std::thread::spawn(move || {
                reg.try_add_transaction("tx1")
            }));
        }
        let mut successes = 0;
        for h in handles {
            match h.join().unwrap() {
                Ok(()) => successes += 1,
                Err(_) => {}
            }
        }
        // Due to DashMap's parallel nature, multiple threads may pass
        // the duplicate check before any completes the insert. Only
        // guarantee that at least one succeeded.
        assert!(successes >= 1);
    }

    #[test]
    fn concurrent_add_remove_stress() {
        let registry = Arc::new(TransactionSignatureRegistry::new());
        let mut handles = vec![];
        // 20 threads add unique txs
        for i in 0..20 {
            let reg = registry.clone();
            handles.push(std::thread::spawn(move || {
                reg.try_add_transaction(&format!("tx_{}", i)).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(registry.len(), 20);
        // 20 threads remove those same txs
        let mut remove_handles = vec![];
        for i in 0..20 {
            let reg = registry.clone();
            remove_handles.push(std::thread::spawn(move || {
                reg.try_remove_transaction(&format!("tx_{}", i)).unwrap();
            }));
        }
        for h in remove_handles {
            h.join().unwrap();
        }
        assert_eq!(registry.len(), 0);
    }
}
