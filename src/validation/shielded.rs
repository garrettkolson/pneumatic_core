//! Shielded validation: structural checks 1-3, nullifier + merkle-root
//! freshness checks (window K), the ZK proof check behind the shared
//! `ShieldedVerifier`, and the fail-closed risk gate.

use super::*;

// ---------------------------------------------------------------------------
// Phase S4.1 — ShieldedValidationSpec
// ---------------------------------------------------------------------------

/// v1 Action circuit capacity: the circuit exposes exactly one nullifier and one
/// output commitment as public inputs (S2.1 / S3.3 Open item #1). The structural
/// check enforces that a tx does not exceed this; a wider circuit moves the cap in
/// this one place (S4.1.3).
const CIRCUIT_MAX_INPUTS: usize = 1;

/// Halo2 instantiation width the Action circuit is built at. `ShieldedVerifier`
/// in `verify.rs` uses the same width, so a proof produced by the prover verifies.
pub(crate) const SHIELDED_VK_K: u32 = 10;

/// A committed pool root plus the height at which it was produced. The merkle-root
/// freshness check (check 3) walks the history oldest→newest.
#[derive(Copy, Clone, Debug)]
pub struct RootSnapshot {
    pub root: [u8; 32],
    pub height: u64,
}

/// The read-only pool view the spec is handed at call time. The spec itself is
/// stateless: the composite node shares one `Arc<ShieldedPool>` (S5.3) that
/// implements both traits; split deployments replay block history (S3.1 / S5.1)
/// and expose it through the same traits.
pub struct ShieldedValidationDeps<'a> {
    /// The spent-nullifier set (check 2).
    pub spent: &'a dyn NullifierMembership,
    /// The committed-root history (check 3).
    pub roots: &'a dyn MerkleRootHistory,
    /// The accepted recency window: a root is fresh if it is at most this many
    /// commits behind the newest.
    pub recency_window: usize,
}

/// The Halo2 verifying key for the Action circuit, built once (lazily) at
/// `SHIELDED_VK_K`. The template circuit supplies only the *structure* `keygen_vk`
/// needs (keygen never uses witness data), so repeated verification reuses one
/// cached `(Params, VerifyingKey)` via `ShieldedVerifier`'s module-level cache.
///
/// SLOW-TEST NOTE: every test that reaches check 4 (this static) is `#[ignore]`d
/// because the first touch pays the one-time ActionCircuit keygen (measured
/// ~2.5 min) and the rest block on it. Un-ignore a test when you change the
/// functionality it covers — each test's ignore note names the trigger and the
/// re-run command. Until then the default suite stays fast.
pub(crate) static SHIELDED_VALIDATOR_VERIFIER: Lazy<ShieldedVerifier> = Lazy::new(|| {
    ShieldedVerifier::new(action_circuit_for_verifying_key(), SHIELDED_VK_K)
        .expect("shielded verifying key (ActionCircuit at K=10)")
});

/// The shielded validation spec (`"Shielded"`). Implements the four fail-closed
/// checks (S4.1: structural, nullifier set, merkle-root freshness, proof) plus a
/// fixed neutral risk. Stateless w.r.t. the pool — it reads pool state through the
/// deps it is handed.
#[derive(Debug, Clone)]
pub struct ShieldedValidationSpec {
    name: String,
}

impl ShieldedValidationSpec {
    pub fn new() -> Self {
        ShieldedValidationSpec {
            name: String::from(Self::NAME),
        }
    }

    /// Spec name — registered under `"Shielded"`.
    pub const NAME: &str = "Shielded";
}

impl Default for ShieldedValidationSpec {
    fn default() -> Self {
        Self::new()
    }
}

/// Fail-closed for a shielded tx whose wire fields cannot be decoded into the
/// circuit's public inputs (check 1's `InvalidCommitment`).
fn shielded_structure_err() -> PneumaticError {
    PneumaticError::Validation(vec![ValidationFailureReason::InvalidCommitment])
}

/// The fixed, neutral risk a shielded tx reports: the amount and parties are
/// unknown by design, so use a constant 2-party / 0-amount factor. The
/// environment's `max_risk` gate still applies to it.
fn shielded_neutral_risk() -> TransactionRiskFactor {
    TransactionRiskFactor {
        affected_parties: 2,
        amount: 0,
        is_contract: false,
        is_multi_party: false,
    }
}

/// Check 1 (structural): counts in bounds, all three commitment vectors non-empty,
/// and nullifiers pairwise distinct within the tx. Fails closed as
/// `InvalidCommitment`.
fn check_structural(tx: &ShieldedTransaction) -> Result<(), PneumaticError> {
    let err = shielded_structure_err;
    if tx.nullifiers.is_empty() || tx.nullifiers.len() > CIRCUIT_MAX_INPUTS {
        return Err(err());
    }
    if tx.spent_commitments.is_empty() || tx.spent_commitments.len() > CIRCUIT_MAX_INPUTS {
        return Err(err());
    }
    if tx.commitments.is_empty() || tx.commitments.len() > CIRCUIT_MAX_INPUTS {
        return Err(err());
    }
    // nullifiers pairwise distinct within the tx (the double-spend guard starts
    // here, before the global nullifier set is consulted).
    for i in 0..tx.nullifiers.len() {
        for j in (i + 1)..tx.nullifiers.len() {
            if tx.nullifiers[i] == tx.nullifiers[j] {
                return Err(err());
            }
        }
    }
    Ok(())
}

/// Check 3 (merkle-root freshness): the referenced root must equal the newest
/// root or lie within `recency_window` commits of it. Unknown or stale roots fail
/// closed (return false → the caller rejects with `StaleMerkleRoot`).
pub(crate) fn is_root_fresh(merkle_root: &Fp, deps: &ShieldedValidationDeps) -> bool {
    let history = deps.roots.root_history();
    if history.is_empty() {
        // No committed root state at all: a root cannot be proven fresh.
        return false;
    }
    let newest = *history.last().unwrap();
    match history.iter().find(|s| s.root == merkle_root.to_repr()) {
        Some(idx) => (newest.height.saturating_sub(idx.height)) <= deps.recency_window as u64,
        None => false,
    }
}

impl TransactionValidationSpec for ShieldedValidationSpec {
    fn validate(
        &self,
        _tx: &Transaction,
        _token: &Token,
        _env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        // The shielded spec validates a `ShieldedTransaction`, never a plain
        // `Transaction`. A plain-tx reaching here is a wiring bug — fail closed.
        Err(PneumaticError::Validation(vec![ValidationFailureReason::UnsupportedAction]))
    }

    fn calculate_risk(&self, _tx: &Transaction) -> TransactionRiskFactor {
        shielded_neutral_risk()
    }

    fn name(&self) -> &str {
        Self::NAME
    }

    fn validate_shielded(
        &self,
        tx: &ShieldedTransaction,
        env_data: &EnvironmentMetadata,
        deps: &ShieldedValidationDeps,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        // Checks run in order 1→2→3→4; the first failure wins (short-circuit).
        // Check 1 — structural bounds.
        check_structural(tx)?;

        // Reconstruct the circuit public inputs (undecodable element → InvalidCommitment).
        let public_inputs = public_inputs_from_shielded_tx(tx)?;

        // Check 2 — each nullifier must be unspent (the double-spend guard).
        for nullifier in &tx.nullifiers {
            if deps.spent.contains_nullifier(*nullifier) {
                return Err(PneumaticError::Validation(vec![ValidationFailureReason::StaleNullifier]));
            }
        }

        // Check 3 — referenced root must be within the recency window.
        if !is_root_fresh(&public_inputs.merkle_root, deps) {
            return Err(PneumaticError::Validation(vec![ValidationFailureReason::StaleMerkleRoot]));
        }

        // Policy gate — the environment's max_risk is enforced before the expensive
        // Halo2 proof check: a tx the policy rejects is dropped early (fail fast),
        // never paying to verify its proof. The neutral risk (score 0.30) is still
        // gated, so a too-low max_risk rejects here.
        let risk = shielded_neutral_risk();
        if risk.score() > env_data.max_risk {
            return Err(PneumaticError::Validation(vec![ValidationFailureReason::RiskExceedsThreshold]));
        }

        // Check 4 — the Halo2 proof over the reconstructed public inputs.
        if SHIELDED_VALIDATOR_VERIFIER.verify(&tx.proof, &public_inputs).is_err() {
            return Err(PneumaticError::Validation(vec![ValidationFailureReason::InvalidShieldedProof]));
        }

        // Success: fixed neutral risk, gated on the environment's max_risk.
        Ok(TransactionValidationResult::valid(vec![], risk))
    }
}
