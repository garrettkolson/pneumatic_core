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

pub mod nullifiers;
pub mod pending;
pub mod signatures;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    pub mod helpers;
    mod nullifiers;
    mod pending;
    mod signatures;
}
