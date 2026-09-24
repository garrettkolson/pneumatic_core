//! `NullifierRegistry`: double-spend nullifier marking (single +
//! atomic batch) and the `NullifierMembership` impl behind it.

use super::*;

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
