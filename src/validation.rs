use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use ff::PrimeField;
use halo2_proofs::pasta::Fp;
use serde::Serialize;
use crate::data::DataProvider;
use crate::environment::EnvironmentMetadata;
use crate::errors::{ValidationFailureReason, TransactionRiskFactor, PneumaticError, ReconciledSignatures};
use crate::shielded::{
    action_circuit_for_verifying_key, PublicInputs, ShieldedVerifier, public_inputs_from_shielded_tx,
};
use crate::tokens::Token;
use crate::transactions::{ShieldedTransaction, Transaction, TransactionValidationResult};

// ---------------------------------------------------------------------------
// TransactionValidationSpec — action-based validation trait
// ---------------------------------------------------------------------------

/// Trait for validating transactions. Implemented by concrete specs
/// (SelfSigned, Executed, etc.) and registered by action name.
pub trait TransactionValidationSpec: Send + Sync {
    /// Validate a transaction against this spec. Returns a result with
    /// risk metrics and assigned finalizer key.
    fn validate(
        &self,
        tx: &Transaction,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError>;

    /// Compute risk metrics for a transaction.
    fn calculate_risk(&self, tx: &Transaction) -> TransactionRiskFactor;

    /// Return the spec name for registration lookup.
    fn name(&self) -> &str;

    /// Optional shielded-transaction validation. The default impl **fails closed**
    /// (`UnsupportedAction`): a spec that does not override this cannot validate a
    /// shielded tx, so a `"ShieldedTransfer"` reaching an unprepared spec is
    /// rejected (mirrors Phase 3.2's reject-unknown-validator). Only
    /// `ShieldedValidationSpec` overrides it. See Phase S4.1.
    fn validate_shielded(
        &self,
        _tx: &ShieldedTransaction,
        _env_data: &EnvironmentMetadata,
        _deps: &ShieldedValidationDeps,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        Err(PneumaticError::Validation(vec![ValidationFailureReason::UnsupportedAction]))
    }
}

// ---------------------------------------------------------------------------
// SelfSignedBlockValidatorSpec — self-validated tokens
// ---------------------------------------------------------------------------

/// Spec for tokens where the owner IS the transaction authority.
/// Transactions pass validation without Executor or Finalizer involvement.
/// Sets `is_self_verified = true` on the token.
#[derive(Debug, Clone)]
pub struct SelfSignedBlockValidatorSpec {
    name: String,
}

impl SelfSignedBlockValidatorSpec {
    pub fn new() -> Self {
        SelfSignedBlockValidatorSpec {
            name: String::from("SelfSigned"),
        }
    }
}

impl Default for SelfSignedBlockValidatorSpec {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionValidationSpec for SelfSignedBlockValidatorSpec {
    fn validate(
        &self,
        tx: &Transaction,
        token: &Token,
        _env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        // Check that the transaction sender is the token owner. The owner is
        // stored as a hex string in metadata (AUDIT 5.9), so decode it to the
        // real key bytes and compare against `tx.sender`. A missing owner fails
        // closed; an unparseable owner also fails closed as NotTokenOwner — never
        // `unwrap`/`from_utf8` on the decode path, since a real 32-byte Ed25519
        // key is not valid UTF-8.
        let token_owner = match token.metadata.get("owner").map(|o| o.as_str()) {
            Some(hex_owner) => hex::decode(hex_owner)
                .map_err(|_| PneumaticError::Validation(vec![ValidationFailureReason::NotTokenOwner]))?,
            None => return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::NotTokenOwner
            ])),
        };

        if tx.sender != token_owner {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::NotTokenOwner
            ]));
        }

        let risk = self.calculate_risk(tx);
        // Self-signed tokens have no finalizer — empty key signals skip
        Ok(TransactionValidationResult::valid(vec![], risk))
    }

    fn calculate_risk(&self, tx: &Transaction) -> TransactionRiskFactor {
        TransactionRiskFactor {
            affected_parties: if !tx.receiver.is_empty() { 2 } else { 1 },
            amount: tx.amount.unwrap_or(0),
            is_contract: tx.action.contains("Contract"),
            is_multi_party: false,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// BlockValidatorSpec implementation for self-signed blocks

impl BlockValidatorSpec for SelfSignedBlockValidatorSpec {
    fn validate(
        &self,
        block: &crate::blocks::Block,
        token: &Token,
        _env_data: &EnvironmentMetadata,
    ) -> Result<BlockValidationResult, PneumaticError> {
        // Validate chain integrity (delegate to the token's blockchain)
        if !token.blockchain.validate_next_block(&block) {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::NotSelfVerified,
            ]));
        }

        // Self-signed tokens must be flagged as self-verified
        if !token.is_self_verified {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::NotSelfVerified,
            ]));
        }

        Ok(BlockValidationResult::Valid)
    }
}

// ---------------------------------------------------------------------------
// ExecutedBlockValidatorSpec — standard executed/processed blocks
// ---------------------------------------------------------------------------

/// Spec for transactions that go through Executor and Finalizer.
/// Validates that the block was properly executed and signed.
#[derive(Debug, Clone)]
pub struct ExecutedBlockValidatorSpec {
    name: String,
    /// Minimum stake required for the transaction
    min_stake: u64,
}

impl ExecutedBlockValidatorSpec {
    pub fn new(min_stake: u64) -> Self {
        ExecutedBlockValidatorSpec {
            name: String::from("Executed"),
            min_stake,
        }
    }
}

impl Default for ExecutedBlockValidatorSpec {
    fn default() -> Self {
        Self::new(0)
    }
}

impl TransactionValidationSpec for ExecutedBlockValidatorSpec {
    fn validate(
        &self,
        tx: &Transaction,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        // The owner gate now lives only on the SelfSigned path (AUDIT 5.9); the
        // Executed spec is owner-agnostic. `token` is retained for the trait
        // signature but not consulted here.
        let _ = token;

        // Validate basic transaction fields
        let mut failures = Vec::new();

        if tx.sender.is_empty() {
            failures.push(ValidationFailureReason::SenderMissing);
        }

        // Phase 5.6 / M12: amount must be present and nonzero. An `Option<u64>`
        // `None` is rejected at admission (wire-compat: keep it serialized as Option);
        // a zero amount is rejected as before.
        if tx.amount.is_none() || tx.amount == Some(0) {
            failures.push(ValidationFailureReason::InvalidAmount);
        }

        if tx.sequence_number == 0 {
            failures.push(ValidationFailureReason::InvalidNonce);
        }

        if !failures.is_empty() {
            return Err(PneumaticError::Validation(failures));
        }

        let risk = self.calculate_risk(tx);

        // Phase 5.7 / H6: real risk gate — reject the transaction when its
        // composite risk score exceeds the environment's configured max_risk
        // (0.0-1.0). The placeholder that compared risk against
        // override_quorum_percentage (a ~67 quorum value a 0.0-1.0 score can
        // never exceed) is removed.
        if risk.score() > env_data.max_risk {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::RiskExceedsThreshold
            ]));
        }

        // Return the finalizer key — in practice this is assigned by the
        // Sentinel after checking stake thresholds
        Ok(TransactionValidationResult::valid(vec![], risk))
    }

    fn calculate_risk(&self, tx: &Transaction) -> TransactionRiskFactor {
        TransactionRiskFactor {
            affected_parties: if !tx.receiver.is_empty() { 2 } else { 1 },
            amount: tx.amount.unwrap_or(0),
            is_contract: tx.action.contains("Contract"),
            is_multi_party: false,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// BlockValidatorSpec implementation for executed blocks

impl BlockValidatorSpec for ExecutedBlockValidatorSpec {
    fn validate(
        &self,
        block: &crate::blocks::Block,
        _token: &Token,
        _env_data: &EnvironmentMetadata,
    ) -> Result<BlockValidationResult, PneumaticError> {
        // Executed transactions must have a result hash (executor ran)
        if block.signed_trans.transaction.result_hash.is_empty() {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::MissingResultHash,
            ]));
        }

        // Executed transactions must have executor signatures
        if block.signed_trans.executor_sigs.is_empty() {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::MissingExecutorSignatures,
            ]));
        }

        // Executed transactions must have a finalizer signature
        if block.signed_trans.finalizer_sig.signature.is_empty() {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::MissingFinalizerSignature,
            ]));
        }

        Ok(BlockValidationResult::Valid)
    }
}

// ---------------------------------------------------------------------------
// BlockValidatorSpec — validates entire blocks (used by Committers/Archivers)
// ---------------------------------------------------------------------------

/// Trait for validating blocks during commit or archiving.
pub trait BlockValidatorSpec: Send + Sync {
    fn validate(
        &self,
        block: &crate::blocks::Block,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<BlockValidationResult, PneumaticError>;
}

#[derive(Debug)]
pub enum BlockValidationResult {
    Valid,
    Invalid(Vec<ValidationFailureReason>),
}

// ---------------------------------------------------------------------------
// ValidationSpecRegistry — stores and looks up specs by action name
// ---------------------------------------------------------------------------

/// Registry of TransactionValidationSpec instances, keyed by spec name.
/// Used by Sentinels to look up the correct validation spec for each
/// transaction action.
#[derive(Default)]
pub struct ValidationSpecRegistry {
    specs: HashMap<String, Arc<dyn TransactionValidationSpec>>,
}

impl ValidationSpecRegistry {
    pub fn new() -> Self {
        ValidationSpecRegistry {
            specs: HashMap::new(),
        }
    }

    /// Register a validation spec under a name.
    pub fn register(&mut self, spec: Box<dyn TransactionValidationSpec>) {
        let name = spec.name().to_string();
        let spec: Arc<dyn TransactionValidationSpec> = Arc::from(spec);
        self.specs.insert(name, spec);
    }

    /// Look up a spec by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn TransactionValidationSpec>> {
        self.specs.get(name)
    }

    /// Register default specs (SelfSigned and Executed).
    pub fn register_defaults(&mut self) {
        self.register(Box::new(SelfSignedBlockValidatorSpec::new()));
        self.register(Box::new(ExecutedBlockValidatorSpec::new(0)));
    }

    /// Register the shielded validation spec (`"Shielded"`).
    ///
    /// Distinct from `register_defaults`: opt-in only, so a non-shielded
    /// deployment does not construct the Halo2 verifying key. Registered under
    /// the name `ShieldedValidationSpec::NAME`; fail-closed for a `"Shielded"`
    /// tx that reaches an unprepared spec (see the trait default impl).
    pub fn register_shielded(&mut self) {
        self.register(Box::new(ShieldedValidationSpec::new()));
    }
}

// Blanket impl: Box<dyn TransactionValidationSpec> delegates to the inner trait object.
// This allows Arc::new(Box<dyn Spec>) to be used where Arc<dyn Spec> is expected.
impl TransactionValidationSpec for Box<dyn TransactionValidationSpec> {
    fn validate(
        &self,
        tx: &Transaction,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        (**self).validate(tx, token, env_data)
    }

    fn calculate_risk(&self, tx: &Transaction) -> TransactionRiskFactor {
        (**self).calculate_risk(tx)
    }

    fn name(&self) -> &str {
        (**self).name()
    }

    /// Delegate shielded validation so a `ShieldedValidationSpec` held behind a
    /// `Box<dyn TransactionValidationSpec>` still validates shielded txs (the
    /// trait default impl below would otherwise fail it closed).
    fn validate_shielded(
        &self,
        tx: &ShieldedTransaction,
        env_data: &EnvironmentMetadata,
        deps: &ShieldedValidationDeps,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        (**self).validate_shielded(tx, env_data, deps)
    }
}

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
const SHIELDED_VK_K: u32 = 10;

/// A committed pool root plus the height at which it was produced. The merkle-root
/// freshness check (check 3) walks the history oldest→newest.
#[derive(Copy, Clone, Debug)]
pub struct RootSnapshot {
    pub root: [u8; 32],
    pub height: u64,
}

/// Check 2 dependency: the nullifier set's read-side. Implemented by the S4.2
/// `NullifierRegistry`; the spec depends on this trait, never the concrete type,
/// so the check is buildable and testable before the registry exists.
pub trait NullifierMembership {
    fn contains_nullifier(&self, nullifier: [u8; 32]) -> bool;
}

/// Check 3 dependency: the append-only history of committed roots. Implemented by
/// the S4.3 root state.
pub trait MerkleRootHistory {
    /// Roots ordered oldest→newest (the last element is the current tip).
    fn root_history(&self) -> &[RootSnapshot];
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
static SHIELDED_VALIDATOR_VERIFIER: Lazy<ShieldedVerifier> = Lazy::new(|| {
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
fn is_root_fresh(merkle_root: &Fp, deps: &ShieldedValidationDeps) -> bool {
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

// ---------------------------------------------------------------------------
// BlockValidatorSpecRegistry — stores and looks up BlockValidatorSpec instances
// ---------------------------------------------------------------------------

/// Registry of BlockValidatorSpec instances, keyed by spec name.
/// Used by Committers and Archivers to look up the correct block validation
/// spec for each token's blocks.
#[derive(Default)]
pub struct BlockValidatorSpecRegistry {
    specs: HashMap<String, Arc<dyn BlockValidatorSpec>>,
}

impl BlockValidatorSpecRegistry {
    pub fn new() -> Self {
        BlockValidatorSpecRegistry {
            specs: HashMap::new(),
        }
    }

    /// Register a block validator spec under a given name.
    pub fn register(&mut self, name: &str, spec: Box<dyn BlockValidatorSpec>) {
        let spec: Arc<dyn BlockValidatorSpec> = Arc::from(spec);
        self.specs.insert(name.to_string(), spec);
    }

    /// Look up a spec by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn BlockValidatorSpec>> {
        self.specs.get(name)
    }

    /// Register default specs (SelfSigned and Executed).
    pub fn register_defaults(&mut self) {
        self.register("SelfSigned", Box::new(SelfSignedBlockValidatorSpec::new()));
        self.register("Executed", Box::new(ExecutedBlockValidatorSpec::new(0)));
    }
}

// Blanket impl: Box<dyn BlockValidatorSpec> delegates to the inner trait object.
impl BlockValidatorSpec for Box<dyn BlockValidatorSpec> {
    fn validate(
        &self,
        block: &crate::blocks::Block,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<BlockValidationResult, PneumaticError> {
        (**self).validate(block, token, env_data)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::EnvironmentMetadataSpec;
    use crate::transactions::{PendingTransaction, TransactionState};
    use crate::registry::PendingTransactionRegistry;
    use crate::rns::identity::NodeIdentity;
    use crate::crypto::AsymCryptoProvider;
    // Phase S4.1 shielded-validation tests: the note/tree primitives for building
    // well-formed `ShieldedTransaction` fixtures, and the scalar field for them.
    use crate::shielded::{
        ActionCircuit, commit, nullifier, root_to_bytes, ShieldedNote, IncrementalMerkleTree, DEFAULT_DEPTH,
    };
    use group::GroupEncoding;
    use pasta_curves::pallas::Scalar as Fq;

    // --- helpers ---

    fn make_token_with_owner(owner: &[u8]) -> Token {
        let mut token = Token::new();
        // Owner is stored as hex (AUDIT 5.9): the tx-level SelfSigned spec decodes
        // it with hex::decode. Using String::from_utf8 here would break the round
        // trip for any non-UTF-8 key.
        token.set_metadata("owner".to_string(), hex::encode(owner));
        token
    }

    fn make_env_with_defaults() -> EnvironmentMetadata {
        let json = r#"{"environment_id":"test","environment_name":"test",
            "partitions":[{"id":"token-part","partition_type":"Token"},
            {"id":"slush-part","partition_type":"Slush"}],
            "asym_crypto_provider":{"Ed25519":null},"sym_crypto_provider":"sym",
            "serialization_provider":"rmp","quorum_percentage":67.0,
            "override_quorum_percentage":1.0,"max_risk":1.0,
            "allowed_token_types":[],"trans_validation_specs":[],
            "block_validation_specs":[],"log_file":"test.log"}"#;
        let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).unwrap();
        EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec")
    }

    fn make_tx(sender: &[u8], receiver: &[u8], amount: Option<u64>, seq: usize) -> Transaction {
        Transaction {
            id: "t".into(),
            action: "Transfer".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: seq,
            sender: sender.to_vec(),
            receiver: receiver.to_vec(),
            amount,
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        }
    }

    // --- SelfSignedBlockValidatorSpec ---

    #[test]
    fn self_signed_validates_sender_is_owner() {
        let spec = SelfSignedBlockValidatorSpec::new();
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[1, 2, 3], &[], None, 1);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_ok());
        assert!(result.unwrap().is_valid);
    }

    #[test]
    fn self_signed_rejects_sender_not_owner() {
        let spec = SelfSignedBlockValidatorSpec::new();
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[9, 9, 9], &[], None, 1);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_err());
        match result.unwrap_err() {
            PneumaticError::Validation(ref reasons) => {
                let reason_str = format!("{:?}", reasons);
                assert!(reason_str.contains("NotTokenOwner"));
            }
            _ => panic!("expected Validation error"),
        }
    }

    #[test]
    fn self_signed_rejects_missing_owner_metadata() {
        let spec = SelfSignedBlockValidatorSpec::new();
        let token = Token::new(); // no owner metadata
        let tx = make_tx(&[1, 2, 3], &[], None, 1);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_err());
    }

    // --- AUDIT 5.9 discriminators: the owner is a hex-encoded key ---

    #[test]
    fn self_signed_accepts_real_ed25519_key_owner() {
        // Audit acceptance: an owner can execute a self-signed operation. A real
        // Ed25519 pubkey (32 bytes) is the owner, hex-encoded in metadata; the
        // signer is that same key. This only works once the owner is a hex string
        // rather than a raw-key String, since a 32-byte key is not valid UTF-8.
        let identity = NodeIdentity::generate_in_memory();
        let owner_pk = identity.ed25519.public_key().expect("owner public key");

        let mut token = Token::new();
        token.set_metadata("owner".to_string(), hex::encode(&owner_pk));
        let tx = make_tx(&owner_pk, &[], Some(100), 1);
        let env = make_env_with_defaults();
        let result =
            TransactionValidationSpec::validate(&SelfSignedBlockValidatorSpec::new(), &tx, &token, &env);
        assert!(result.is_ok(), "owner-key owner must be accepted, got {result:?}");
    }

    #[test]
    fn self_signed_accepts_non_utf8_key_bytes() {
        // Discriminator for the String-vs-bytes bug: a non-UTF-8 byte payload
        // (0x80 has no valid UTF-8 meaning) can be stored only as hex. The old
        // String::from_utf8(owner) round-trip would panic on it; hex::decode does
        // not, so the owner (these exact bytes) validates as sender.
        let owner: Vec<u8> = vec![0x80u8; 32];
        let mut token = Token::new();
        token.set_metadata("owner".to_string(), hex::encode(&owner));
        let tx = make_tx(&owner, &[], Some(100), 1);
        let env = make_env_with_defaults();
        let result =
            TransactionValidationSpec::validate(&SelfSignedBlockValidatorSpec::new(), &tx, &token, &env);
        assert!(result.is_ok(), "non-UTF-8 key owner must validate, got {result:?}");
    }

    #[test]
    fn self_signed_rejects_unparseable_owner() {
        // Fail closed: an owner that is not valid hex (e.g. a stray non-hex byte
        // from a corrupt write) yields NotTokenOwner, never a panic or accept.
        let mut token = Token::new();
        token.set_metadata("owner".to_string(), "not-hex-value!".to_string());
        let tx = make_tx(&[1, 2, 3], &[], Some(100), 1);
        let env = make_env_with_defaults();
        let result =
            TransactionValidationSpec::validate(&SelfSignedBlockValidatorSpec::new(), &tx, &token, &env);
        assert!(matches!(
            result,
            Err(PneumaticError::Validation(ref reasons))
                if reasons.iter().any(|r| matches!(r, ValidationFailureReason::NotTokenOwner))
        ), "unparseable owner must fail closed as NotTokenOwner, got {result:?}");
    }

    #[test]
    fn executed_is_owner_agnostic_now() {
        // The Executed spec no longer bans `sender == owner` (AUDIT 5.9). A token
        // whose owner equals the sender now validates on the Executed spec like
        // any other token — the owner gate now lives only on the SelfSigned path.
        let spec = ExecutedBlockValidatorSpec::new(0);
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[1, 2, 3], &[9], Some(100), 1);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_ok(), "Executed spec must be owner-agnostic, got {result:?}");
    }

    #[test]
    fn self_signed_risk_with_receiver_counts_two_parties() {
        let spec = SelfSignedBlockValidatorSpec::new();
        let tx = make_tx(&[1], &[2], Some(100), 1);
        let risk = spec.calculate_risk(&tx);
        assert_eq!(risk.affected_parties, 2);
    }

    #[test]
    fn self_signed_risk_no_receiver_counts_one_party() {
        let spec = SelfSignedBlockValidatorSpec::new();
        let tx = make_tx(&[1], &[], Some(100), 1);
        let risk = spec.calculate_risk(&tx);
        assert_eq!(risk.affected_parties, 1);
    }

    // --- ExecutedBlockValidatorSpec ---

    #[test]
    fn executed_rejects_empty_sender() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[], &[], Some(100), 1);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_err());
    }

    #[test]
    fn executed_rejects_zero_amount() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[9, 9, 9], &[], Some(0), 1);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_err());
    }

    #[test]
    fn executed_rejects_null_amount() {
        // Phase 5.6 / M12: `amount: None` must fail the executed-admission gate.
        let spec = ExecutedBlockValidatorSpec::new(0);
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[9, 9, 9], &[], None, 1);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_err());
        if let Err(PneumaticError::Validation(failures)) = result {
            assert!(failures
                .iter()
                .any(|f| matches!(f, ValidationFailureReason::InvalidAmount)));
        }
    }

    #[test]
    fn executed_rejects_zero_nonce() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[9, 9, 9], &[], Some(100), 0);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_err());
    }

    #[test]
    fn executed_allows_valid_transaction() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[9, 9, 9], &[], Some(100), 1);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_ok());
        assert!(result.unwrap().is_valid);
    }

    #[test]
    fn executed_default_construction_zero_min_stake() {
        let spec = ExecutedBlockValidatorSpec::default();
        assert_eq!(spec.name(), "Executed");
    }

    // --- ValidationSpecRegistry ---

    #[test]
    fn registry_registers_and_looks_up_defaults() {
        let mut registry = ValidationSpecRegistry::new();
        registry.register_defaults();
        assert!(registry.get("SelfSigned").is_some());
        assert!(registry.get("Executed").is_some());
    }

    #[test]
    fn registry_get_nonexistent_returns_none() {
        let registry = ValidationSpecRegistry::new();
        assert!(registry.get("Unknown").is_none());
    }

    // --- BlockValidationResult ---

    #[test]
    fn block_validation_result_variants() {
        let valid = BlockValidationResult::Valid;
        let debug_str = format!("{:?}", valid);
        assert!(debug_str.contains("Valid"));

        let invalid = BlockValidationResult::Invalid(vec![ValidationFailureReason::InvalidAmount]);
        let debug_str = format!("{:?}", invalid);
        assert!(debug_str.contains("Invalid"));
    }

    // --- T08: Self-Validated Token Flow (minimal integration) ---

    #[test]
    fn self_signed_token_flow_end_to_end() {
        // Create token with owner metadata. Owner is stored as hex (AUDIT 5.9)
        // so hex::decode(b"alice") round-trips to the sender's real key bytes.
        let mut token = Token::new();
        token.set_metadata("owner".to_string(), hex::encode(b"alice"));

        // Create transaction with matching sender
        let tx = Transaction {
            id: "tx_self_signed".into(),
            action: "Transfer".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b"alice".to_vec(),
            receiver: vec![],
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        };

        // Validate with SelfSigned spec
        let spec = SelfSignedBlockValidatorSpec::new();
        let env = make_env_with_defaults();
        let validation_result = TransactionValidationSpec::validate(&spec, &tx, &token, &env).unwrap();
        assert!(validation_result.is_valid);

        // Transition to Validated state in PendingTransaction
        let pt_id = tx.id.clone();
        let mut pt = PendingTransaction::new(pt_id.clone(), TransactionState::Pending);
        pt.transition_to_validated(tx, validation_result);

        // Confirm registry holds validated state
        let registry = PendingTransactionRegistry::new();
        registry.add_transaction(pt_id.clone(), pt).unwrap();
        let result = registry.get_validation_result(&pt_id).unwrap();
        assert!(result.is_valid);
    }

    // --- Nonce tests ---

    #[test]
    fn nonce_zero_rejected_by_executed_spec() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[9, 9, 9], &[], Some(100), 0);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_err());
    }

    #[test]
    fn nonce_nonzero_accepted_by_executed_spec() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let token = make_token_with_owner(&[1, 2, 3]);
        let tx = make_tx(&[9, 9, 9], &[], Some(100), 1);
        let env = make_env_with_defaults();
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(result.is_ok());
    }

    #[test]
    fn nonce_increasing_validated() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let token = make_token_with_owner(&[1, 2, 3]);
        let env = make_env_with_defaults();

        // seq=1 → valid
        let tx1 = make_tx(&[9, 9, 9], &[], Some(100), 1);
        assert!(TransactionValidationSpec::validate(&spec, &tx1, &token, &env).is_ok());

        // seq=2 → also valid
        let tx2 = make_tx(&[9, 9, 9], &[], Some(200), 2);
        assert!(TransactionValidationSpec::validate(&spec, &tx2, &token, &env).is_ok());
    }

    // --- Test helpers for block-level validators ---

    use crate::blocks::{BlockFactory, Blockchain};
    use crate::transactions::{SignedTransaction, TransactionSignature};
    use crate::validation::BlockValidatorSpecRegistry;
    use std::collections::HashMap;

    fn make_signed_tx_with_fields(
        result_hash: Vec<u8>,
        executor_sigs: HashMap<Vec<u8>, TransactionSignature>,
        finalizer_sig: TransactionSignature,
    ) -> SignedTransaction {
        SignedTransaction {
            shielded: None,
            transaction_id: String::from("test_signed_tx"),
            transaction: Transaction {
                id: String::from("test_tx"),
                action: String::from("Transfer"),
                token_id: vec![1],
                bid: None,
                sequence_number: 1,
                sender: vec![1, 2, 3],
                receiver: vec![],
                amount: Some(100),
                timestamp: 0,
                result_hash,
                sender_signature: vec![],
            },
            total_stake: 42,
            total_voters: 3,
            leader_address: vec![1],
            leader_stake: 24,
            leader_hash: vec![0u8; 32],
            finalizer_addr: vec![2],
            finalizer_sig,
            executor_sigs,
            proposer_key: vec![1],
        }
    }

    fn make_valid_block(signed_tx: SignedTransaction, blockchain: &mut Blockchain) -> crate::blocks::Block {
        let proposer_key = signed_tx.proposer_key.clone();
        // Pre-seed a genesis block so these tests exercise the
        // non-empty-chain path
        if blockchain.get_count() == 0 {
            let genesis = SignedTransaction {
                shielded: None,
                transaction_id: String::from("genesis"),
                transaction: Transaction {
                    id: String::from("genesis"),
                    action: String::from("Genesis"),
                    token_id: vec![],
                    bid: None,
                    sequence_number: 0,
                    sender: vec![],
                    receiver: vec![],
                    amount: None,
                    timestamp: 0,
                    result_hash: vec![],
                    sender_signature: vec![],
                },
                total_stake: 42,
                total_voters: 3,
                leader_address: vec![1],
                leader_stake: 24,
                leader_hash: signed_tx.leader_hash.clone(),
                finalizer_addr: vec![2],
                finalizer_sig: TransactionSignature {
                    transaction_id: vec![],
                    env_id: vec![],
                    transaction_hash: vec![],
                    signature: vec![0u8; 64],
                    current_stake: 10,
                },
                executor_sigs: HashMap::new(),
                proposer_key: vec![1],
            };
            let mut gen_block = crate::blocks::Block {
                signed_trans: genesis,
                token_metadata: HashMap::new(),
                previous_hash: signed_tx.leader_hash.clone(),
                timestamp: 0,
                current_hash: vec![],
                finality_status: crate::blocks::FinalityStatus::Optimistic,
                proposer_key: vec![1],
                epoch_number: 0,
            };
            gen_block.current_hash =
                BlockFactory::create_hash(&gen_block).expect("well-formed test block hash");
            blockchain.add_block(gen_block);
        }
        let prev_hash = blockchain.get_current_chain_state().last_hash_in;
        let mut block = crate::blocks::Block {
            signed_trans: signed_tx,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: crate::blocks::FinalityStatus::Optimistic,
            proposer_key,
            epoch_number: 0,
        };
        block.current_hash =
            BlockFactory::create_hash(&block).expect("well-formed test block hash");
        block
    }

    fn make_self_signed_token(owner: &[u8]) -> Token {
        let mut token = Token::new();
        token.set_metadata("owner".to_string(), hex::encode(owner));
        token.is_self_verified = true;
        token.block_validation_spec_name = String::from("SelfSigned");
        token
    }

    // --- SelfSignedBlockValidatorSpec (BlockValidatorSpec trait) ---

    #[test]
    fn self_signed_block_validates_chain_and_self_verified() {
        let spec = SelfSignedBlockValidatorSpec::new();
        let mut token = make_self_signed_token(&[1, 2, 3]);
        let signed_tx = make_signed_tx_with_fields(vec![], HashMap::new(), TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![0u8; 64],
            current_stake: 10,
        });
        let block = make_valid_block(signed_tx, &mut token.blockchain);
        let env = make_env_with_defaults();
        let result = BlockValidatorSpec::validate(&spec, &block, &token, &env);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), BlockValidationResult::Valid));
    }

    #[test]
    fn self_signed_block_rejects_non_self_verified_token() {
        let spec = SelfSignedBlockValidatorSpec::new();
        let mut token = Token::new();
        token.set_metadata("owner".to_string(), hex::encode(vec![1, 2, 3]));
        token.is_self_verified = false;
        token.block_validation_spec_name = String::from("SelfSigned");
        let signed_tx = make_signed_tx_with_fields(vec![], HashMap::new(), TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![0u8; 64],
            current_stake: 10,
        });
        let block = make_valid_block(signed_tx, &mut token.blockchain);
        let env = make_env_with_defaults();
        let result = BlockValidatorSpec::validate(&spec, &block, &token, &env);
        assert!(result.is_err());
        match result.unwrap_err() {
            PneumaticError::Validation(ref reasons) => {
                let reason_str = format!("{:?}", reasons);
                assert!(reason_str.contains("NotSelfVerified"));
            }
            _ => panic!("expected Validation error"),
        }
    }

    // --- ExecutedBlockValidatorSpec (BlockValidatorSpec trait) ---

    #[test]
    fn executed_block_validates_all_requirements() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let mut token = Token::new();
        token.set_metadata("owner".to_string(), hex::encode(vec![1, 2, 3]));
        token.is_self_verified = false;
        token.block_validation_spec_name = String::from("Executed");

        let mut executor_sigs = HashMap::new();
        executor_sigs.insert(vec![10, 20], TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![0u8; 64],
            current_stake: 10,
        });

        let signed_tx = make_signed_tx_with_fields(
            vec![1, 2, 3, 4],
            executor_sigs,
            TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![0u8; 64],
                current_stake: 10,
            },
        );
        let block = make_valid_block(signed_tx, &mut token.blockchain);
        let env = make_env_with_defaults();
        let result = BlockValidatorSpec::validate(&spec, &block, &token, &env);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), BlockValidationResult::Valid));
    }

    #[test]
    fn executed_block_rejects_missing_result_hash() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let mut token = Token::new();
        token.set_metadata("owner".to_string(), hex::encode(vec![1, 2, 3]));
        token.block_validation_spec_name = String::from("Executed");

        let signed_tx = make_signed_tx_with_fields(
            vec![],
            HashMap::new(),
            TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![0u8; 64],
                current_stake: 10,
            },
        );
        let block = make_valid_block(signed_tx, &mut token.blockchain);
        let env = make_env_with_defaults();
        let result = BlockValidatorSpec::validate(&spec, &block, &token, &env);
        assert!(result.is_err());
        match result.unwrap_err() {
            PneumaticError::Validation(ref reasons) => {
                let reason_str = format!("{:?}", reasons);
                assert!(reason_str.contains("MissingResultHash"));
            }
            _ => panic!("expected Validation error"),
        }
    }

    #[test]
    fn executed_block_rejects_missing_executor_sigs() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let mut token = Token::new();
        token.set_metadata("owner".to_string(), hex::encode(vec![1, 2, 3]));
        token.block_validation_spec_name = String::from("Executed");

        let signed_tx = make_signed_tx_with_fields(
            vec![1, 2, 3, 4],
            HashMap::new(), // empty executor_sigs
            TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![0u8; 64],
                current_stake: 10,
            },
        );
        let block = make_valid_block(signed_tx, &mut token.blockchain);
        let env = make_env_with_defaults();
        let result = BlockValidatorSpec::validate(&spec, &block, &token, &env);
        assert!(result.is_err());
        match result.unwrap_err() {
            PneumaticError::Validation(ref reasons) => {
                let reason_str = format!("{:?}", reasons);
                assert!(reason_str.contains("MissingExecutorSignatures"));
            }
            _ => panic!("expected Validation error"),
        }
    }

    #[test]
    fn executed_block_rejects_missing_finalizer_signature() {
        let spec = ExecutedBlockValidatorSpec::new(0);
        let mut token = Token::new();
        token.set_metadata("owner".to_string(), hex::encode(vec![1, 2, 3]));
        token.block_validation_spec_name = String::from("Executed");

        let mut executor_sigs = HashMap::new();
        executor_sigs.insert(vec![10, 20], TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![0u8; 64],
            current_stake: 10,
        });

        let signed_tx = make_signed_tx_with_fields(
            vec![1, 2, 3, 4],
            executor_sigs,
            TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![], // empty finalizer signature
                current_stake: 10,
            },
        );
        let block = make_valid_block(signed_tx, &mut token.blockchain);
        let env = make_env_with_defaults();
        let result = BlockValidatorSpec::validate(&spec, &block, &token, &env);
        assert!(result.is_err());
        match result.unwrap_err() {
            PneumaticError::Validation(ref reasons) => {
                let reason_str = format!("{:?}", reasons);
                assert!(reason_str.contains("MissingFinalizerSignature"));
            }
            _ => panic!("expected Validation error"),
        }
    }

    // --- BlockValidatorSpecRegistry ---

    #[test]
    fn block_validator_registry_registers_and_looks_up_defaults() {
        let mut registry = BlockValidatorSpecRegistry::new();
        registry.register_defaults();
        assert!(registry.get("SelfSigned").is_some());
        assert!(registry.get("Executed").is_some());
    }

    #[test]
    fn block_validator_registry_get_nonexistent_returns_none() {
        let registry = BlockValidatorSpecRegistry::new();
        assert!(registry.get("Unknown").is_none());
    }

    // --- Phase 5.7 / H6: the risk gate enforces max_risk ---

    #[test]
    fn executed_block_validator_rejects_tx_over_max_risk() {
        // A contract tx with a large amount scores 0.85 (amount_risk 1.0 +
        // party_risk 0.5 + complexity 1.0, weighted). With max_risk = 0.5 it
        // must be rejected at the risk gate. The old placeholder compared risk
        // against override_quorum_percentage (= 1.0 here); 0.85 > 1.0 is false,
        // so that gate would have accepted this tx — proving the gate now truly
        // enforces max_risk.
        let mut env = make_env_with_defaults();
        env.max_risk = 0.5;
        let spec = ExecutedBlockValidatorSpec::new(0);
        let mut tx = make_tx(b"sender", b"receiver", Some(2_000_000_000), 7);
        tx.action = "ContractProcess".into();
        let token = Token::new();
        // Disambiguate the two `validate` impls by calling the transaction
        // spec trait explicitly.
        let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
        assert!(matches!(
            result,
            Err(PneumaticError::Validation(reasons))
                if reasons.iter().any(|reason| {
                    matches!(reason, ValidationFailureReason::RiskExceedsThreshold)
                })
        ));
    }

    #[test]
    fn executed_block_validator_allows_tx_under_max_risk() {
        // A small 2-party transfer scores 0.30 (amount 0.0 + party 0.5 +
        // complexity 0.5). With max_risk = 0.9 it must pass.
        let mut env = make_env_with_defaults();
        env.max_risk = 0.9;
        let spec = ExecutedBlockValidatorSpec::new(0);
        let tx = make_tx(b"sender", b"receiver", Some(100), 7);
        let token = Token::new();
        assert!(matches!(
            TransactionValidationSpec::validate(&spec, &tx, &token, &env),
            Ok(_)
        ));
    }

    // -----------------------------------------------------------------------
    // Phase S4.1 — ShieldedValidationSpec (four fail-closed checks)
    // -----------------------------------------------------------------------

    /// Nullifier-set fake implementing the `NullifierMembership` interface so
    /// check 2 can run against in-memory state (S4.1.5) without the S4.2 type.
    #[derive(Default)]
    struct FakeNullifier {
        spent: Vec<[u8; 32]>,
    }
    impl NullifierMembership for FakeNullifier {
        fn contains_nullifier(&self, nullifier: [u8; 32]) -> bool {
            self.spent.iter().any(|n| *n == nullifier)
        }
    }

    /// Root-history fake implementing the `MerkleRootHistory` interface so check 3
    /// can walk a known history (S4.1.5) without the S4.3 type.
    #[derive(Default)]
    struct FakeRootHistory {
        roots: Vec<RootSnapshot>,
    }
    impl MerkleRootHistory for FakeRootHistory {
        fn root_history(&self) -> &[RootSnapshot] {
            &self.roots
        }
    }

    /// Build a well-formed, fully-decodable 1-in/1-out `ShieldedTransaction`. The
    /// commitment, nullifier and root bytes are real (so `point_coords` /
    /// `bytes_to_root` decode cleanly); only the `proof` is a placeholder buffer.
    /// Checks 1→3 short-circuit before the proof (check 4), so the placeholder is
    /// never exercised by the rejection discriminators; the test that must reach
    /// check 4 deliberately feeds it and expects `InvalidShieldedProof`.
    fn make_shielded_tx() -> ShieldedTransaction {
        make_shielded_tx_with(vec![0u8; 64], 0)
    }

    /// Same fixture with an explicit proof buffer and fee. The fee must match the
    /// value the verifier's proof is built over (check 4's `public_inputs.fee`).
    fn make_shielded_tx_with(proof: Vec<u8>, fee: u64) -> ShieldedTransaction {
        let note = ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) };
        let spend_key = [0xABu8; 32];
        let output_note = ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3), rcm: Fq::from(4) };
        let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        let (root, _proof) = tree.append(&commit(&note));
        let spent_commit: [u8; 32] = commit(&note).to_bytes().as_ref().try_into().unwrap();
        let output_commit: [u8; 32] = commit(&output_note).to_bytes().as_ref().try_into().unwrap();
        let nullifier_bytes = nullifier(&note, &spend_key);
        ShieldedTransaction {
            id: "shielded_test".into(),
            action: "ShieldedTransfer".into(),
            token_id: vec![1],
            spent_commitments: vec![spent_commit],
            nullifiers: vec![nullifier_bytes],
            commitments: vec![output_commit],
            merkle_root: root_to_bytes(&root),
            proof,
            note_ciphertexts: vec![vec![0u8; 64]],
            fee,
        }
    }

    /// Run a tx through `validate_shielded` with the shared test environment
    /// (max_risk = 1.0). The fakes are owned by the caller so the borrowed deps
    /// stay in scope for the duration of the check.
    fn run_shielded(
        tx: &ShieldedTransaction,
        spent: &FakeNullifier,
        roots: &FakeRootHistory,
        window: usize,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        let deps = ShieldedValidationDeps { spent, roots, recency_window: window };
        ShieldedValidationSpec::new().validate_shielded(tx, &make_env_with_defaults(), &deps)
    }

    fn reason_matches(result: &Result<TransactionValidationResult, PneumaticError>, r: ValidationFailureReason) -> bool {
        matches!(
            result,
            Err(PneumaticError::Validation(ref rs)) if rs.iter().any(|reason| *reason == r)
        )
    }

    #[test]
    fn register_shielded_adds_spec_without_touching_defaults() {
        // S4.1.3 discriminator: register_defaults() alone leaves no "Shielded" spec
        // (a shielded tx would then fall to the fail-closed default); register_shielded()
        // adds it and does not alter the existing defaults.
        let mut reg = ValidationSpecRegistry::new();
        reg.register_defaults();
        assert!(reg.get("Shielded").is_none(), "defaults must not register Shielded");

        reg.register_shielded();
        let spec = reg.get("Shielded").expect("register_shielded adds a Shielded spec");
        assert_eq!(spec.name(), "Shielded");

        // Defaults byte-identical.
        assert!(reg.get("SelfSigned").is_some());
        assert!(reg.get("Executed").is_some());
    }

    #[test]
    fn unregistered_shielded_action_fails_closed() {
        // S4.1.6 discriminator: the trait default impl rejects shielded txs for any
        // non-shielded spec. Exercise it directly on a plain spec — a shielded tx
        // routed to the wrong spec must fail closed (UnsupportedAction), never accept.
        let spec = ExecutedBlockValidatorSpec::new(0);
        let tx = make_shielded_tx();
        let spent = FakeNullifier::default();
        let roots = FakeRootHistory::default();
        let env = make_env_with_defaults();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &roots, recency_window: 10 };
        let result = TransactionValidationSpec::validate_shielded(&spec, &tx, &env, &deps);
        assert!(reason_matches(&result, ValidationFailureReason::UnsupportedAction),
            "a non-shielded spec must fail closed on validate_shielded");
    }

    #[test]
    fn proof_check_rejects_garbage_proof() {
        // S4.1.4 discriminator: a valid-shape tx (checks 1-3 pass) with a garbage
        // proof must reach and fail check 4 as InvalidShieldedProof — proving check 4
        // is real. A structural failure on the same input, by contrast, fails check 1.
        let tx = make_shielded_tx();
        let spent = FakeNullifier::default();
        let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
        let result = run_shielded(&tx, &spent, &roots, 10);
        assert!(reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
            "a valid-shape tx with a garbage proof must reach and fail check 4");
    }

    #[test]
    fn structural_ok_shape_passes_checks_1_through_3() {
        // Same input as above, but with a *valid* proof would pass checks 1-3; here
        // the garbage proof only matters once checks 1-3 pass, so the failure at
        // InvalidShieldedProof itself proves checks 1-3 all passed (revert any one and
        // this input returns InvalidCommitment / StaleNullifier / StaleMerkleRoot).
        let tx = make_shielded_tx();
        let spent = FakeNullifier::default();
        let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
        let result = run_shielded(&tx, &spent, &roots, 10);
        assert!(reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
            "structural + nullifier + root must all pass before the proof check");
        // Sanity: the same tx with a stale nullifier must fail check 2, not check 4.
        let spent2 = FakeNullifier { spent: tx.nullifiers.clone() };
        let result2 = run_shielded(&tx, &spent2, &roots, 10);
        assert!(reason_matches(&result2, ValidationFailureReason::StaleNullifier),
            "an already-spent nullifier must be caught at check 2, before the proof check");
    }

    #[test]
    fn structural_rejects_empty_nullifier_vec() {
        // S4.1.3 discriminator: an empty nullifier vector fails check 1 (InvalidCommitment).
        let mut tx = make_shielded_tx();
        tx.nullifiers = vec![];
        let spent = FakeNullifier::default();
        let roots = FakeRootHistory::default();
        let result = run_shielded(&tx, &spent, &roots, 10);
        assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
            "an empty nullifier vector must fail the structural check");
    }

    #[test]
    fn structural_rejects_empty_spent_commitments() {
        // S4.1.2/1.3 discriminator: the new spent_commitments field is load-bearing —
        // an empty vec fails check 1 (InvalidCommitment), which would otherwise be a
        // check-4 failure.
        let mut tx = make_shielded_tx();
        tx.spent_commitments = vec![];
        let spent = FakeNullifier::default();
        let roots = FakeRootHistory::default();
        let result = run_shielded(&tx, &spent, &roots, 10);
        assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
            "an empty spent_commitments vector must fail the structural check");
    }

    #[test]
    fn structural_rejects_empty_commitments() {
        // S4.1.3 discriminator: an empty output commitment vector fails check 1.
        let mut tx = make_shielded_tx();
        tx.commitments = vec![];
        let spent = FakeNullifier::default();
        let roots = FakeRootHistory::default();
        let result = run_shielded(&tx, &spent, &roots, 10);
        assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
            "an empty commitments vector must fail the structural check");
    }

    #[test]
    fn structural_rejects_nullifier_count_over_circuit_cap() {
        // S4.1.3 discriminator: a tx with more nullifiers than the v1 circuit cap
        // (1-in) fails check 1 (InvalidCommitment). This is the S4.1.3 cap-enforcement.
        let mut tx = make_shielded_tx();
        tx.nullifiers.push(tx.nullifiers[0]); // two identical entries → over the cap
        let spent = FakeNullifier::default();
        let roots = FakeRootHistory::default();
        let result = run_shielded(&tx, &spent, &roots, 10);
        assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
            "nullifiers over the v1 circuit cap must fail the structural check");
    }

    #[test]
    fn structural_rejects_duplicate_nullifiers() {
        // S4.1.3 discriminator: duplicate nullifiers within a tx fail check 1.
        let mut tx = make_shielded_tx();
        tx.nullifiers.push(tx.nullifiers[0]);
        let spent = FakeNullifier::default();
        let roots = FakeRootHistory::default();
        let result = run_shielded(&tx, &spent, &roots, 10);
        assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
            "duplicate nullifiers must fail the structural check");
    }

    #[test]
    fn nullifier_check_rejects_already_spent() {
        // S4.1.5 discriminator: an already-spent nullifier fails check 2
        // (StaleNullifier) — before the proof check. Same tx, fresh nullifier set,
        // passes check 2 and reaches (and fails) check 4.
        let tx = make_shielded_tx();
        let nullifier = tx.nullifiers[0];
        let spent = FakeNullifier { spent: vec![nullifier] };
        let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
        let result = run_shielded(&tx, &spent, &roots, 10);
        assert!(reason_matches(&result, ValidationFailureReason::StaleNullifier),
            "an already-spent nullifier must be rejected at check 2");
        // Contrast: same tx against an empty set reaches the (garbage) proof check.
        let spent_fresh = FakeNullifier::default();
        let result2 = run_shielded(&tx, &spent_fresh, &roots, 10);
        assert!(reason_matches(&result2, ValidationFailureReason::InvalidShieldedProof),
            "with a fresh nullifier the tx reaches the proof check");
    }

    /// S4.2.4 seam: S4.1's check-2 discriminator re-run against the *concrete*
    /// `NullifierRegistry` (S4.1 ran it against `FakeNullifier`; this proves the
    /// `NullifierMembership` impl lines up on the real type).
    #[test]
    fn check2_against_concrete_nullifier_registry_rejects_spent() {
        use crate::registry::NullifierRegistry;
        let tx = make_shielded_tx();
        let registry = NullifierRegistry::new();
        registry.try_mark_spent(tx.nullifiers[0]).unwrap();
        let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
        let deps = ShieldedValidationDeps { spent: &registry, roots: &roots, recency_window: 10 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::StaleNullifier),
            "check 2 must fire StaleNullifier against the concrete registry"
        );
    }

    /// S4.2.4 seam: with the concrete registry holding the nullifier as *fresh*,
    /// the same tx must clear checks 1-3 and reach (and fail) the check-4 proof
    /// check — the real registry neither rejects a fresh spend nor leaks the
    /// placeholder proof earlier than S4.1's fake did.
    #[test]
    fn check2_against_concrete_registry_fresh_reaches_proof_check() {
        use crate::registry::NullifierRegistry;
        let tx = make_shielded_tx();
        let registry = NullifierRegistry::new(); // fresh: nothing spent
        let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
        let deps = ShieldedValidationDeps { spent: &registry, roots: &roots, recency_window: 10 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
            "a fresh concrete registry must let the tx past check 2 to the proof check"
        );
    }

    #[test]
    fn root_freshness_accepts_within_window_and_rejects_beyond() {
        // S4.1.5 discriminator: the referenced root within the recency window passes
        // check 3 (reaching the proof check); the same root beyond the window is
        // rejected as StaleMerkleRoot — proving the window, not mere equality, is
        // the freshness gate.
        let tx = make_shielded_tx();
        let spent = FakeNullifier::default();

        // History with the tx root (R0) at height 0 and a newer tip (T) at height 1.
        let tip = RootSnapshot { root: [7u8; 32], height: 1 };
        let r0 = RootSnapshot { root: tx.merkle_root, height: 0 };
        let history = FakeRootHistory { roots: vec![r0, tip] };

        // window 1: R0 is 1 commit behind the tip → within window → passes check 3.
        let within = run_shielded(&tx, &spent, &history, 1);
        assert!(reason_matches(&within, ValidationFailureReason::InvalidShieldedProof),
            "a root within the window must pass the freshness check");

        // window 0: the SAME root, SAME history, only the window shrinks → 1 commit
        // behind > window 0 → StaleMerkleRoot.
        let beyond = run_shielded(&tx, &spent, &history, 0);
        assert!(reason_matches(&beyond, ValidationFailureReason::StaleMerkleRoot),
            "a root beyond the window must be rejected as stale");
    }

    // -----------------------------------------------------------------------
    // Phase S4.3 — S4.1's check-3 discriminators re-run against the concrete
    // `MerkleRootState` (S4.3.3). The real `NullifierRegistry` backs
    // `deps.spent` (fresh, so check 2 passes) and deps are built inline:
    // the `run_shielded` helper above is typed to the S4.1 fakes, which stay
    // in place to pin the window arithmetic on a full-history view.
    // -----------------------------------------------------------------------

    use crate::shielded::MerkleRootState;
    use pasta_curves::pallas::Base as Fp;

    /// Distinct, decodable dummy pool roots: `Fp::from(tag)` in canonical
    /// form, so `public_inputs_from_shielded_tx` decodes them. Check 3
    /// compares raw bytes, so the dummies never need to correspond to a
    /// real tree (they must, however, be valid field elements).
    fn dummy_root(tag: u64) -> [u8; 32] {
        Fp::from(tag).to_repr()
    }

    /// S4.3.3 seam: S4.1's "current root accepted" on the concrete type.
    /// State = genesis + one push of the tx's own root (tip, distance 0) →
    /// passes check 3, reaches check 4. The whole set fails to compile
    /// without the `impl MerkleRootHistory` — the seam is provably
    /// load-bearing (S4.2.4 style).
    #[test]
    fn check3_concrete_root_state_current_root_accepted() {
        use crate::registry::NullifierRegistry;
        let tx = make_shielded_tx();
        let mut state = MerkleRootState::new(10);
        state.push(tx.merkle_root); // tip = the root the tx references
        let spent = NullifierRegistry::new(); // fresh: check 2 passes
        let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
            "the current tip root must pass check 3 and reach the proof check"
        );
    }

    /// S4.3.3 seam: the parent's "root from K-1 states back accepted", exact.
    /// The tx references the genesis root (height 0); 9 dummy pushes put the
    /// tip at height 9 → distance 9 = K-1 ≤ 10 → accepted, reaches check 4.
    #[test]
    fn check3_concrete_root_state_k_minus_1_back_accepted() {
        use crate::registry::NullifierRegistry;
        let mut tx = make_shielded_tx();
        tx.merkle_root = [0u8; 32]; // the genesis zero root = the height-0 snapshot
        let mut state = MerkleRootState::new(10);
        for i in 1..=9u64 {
            state.push(dummy_root(i)); // tip at height 9
        }
        let spent = NullifierRegistry::new();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
            "a root K-1 commits behind the tip must pass check 3"
        );
    }

    /// S4.3.3 seam: the boundary discriminator. 10 dummy pushes → tip at
    /// height 10, distance EXACTLY K. The root is still retained (capacity
    /// window+1 = 11, nothing pruned), so the acceptance is decided purely
    /// by the `<=` in `is_root_fresh` (validation.rs:522): an
    /// implementation with `<` fails exactly this test. Together with
    /// S4.1.5's fake test above (which pins the reject side of the same
    /// comparison on a full history), the window math is pinned on both
    /// sides.
    #[test]
    fn check3_concrete_root_state_at_window_boundary_accepted() {
        use crate::registry::NullifierRegistry;
        let mut tx = make_shielded_tx();
        tx.merkle_root = [0u8; 32];
        let mut state = MerkleRootState::new(10);
        for i in 1..=10u64 {
            state.push(dummy_root(i)); // tip at height 10
        }
        let spent = NullifierRegistry::new();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
            "a root exactly K commits behind the tip must pass check 3 (<=, not <)"
        );
    }

    /// S4.3.3 seam: the parent's "K+1 back rejected". 11 dummy pushes →
    /// the 12th entry prunes the height-0 genesis root out → the referenced
    /// root is absent. Per Decision 1, on the bounded state "beyond the
    /// window" and "not found" are the same event; the assertion is the
    /// parent's, the mechanism is the prune.
    #[test]
    fn check3_concrete_root_state_k_plus_1_back_rejected() {
        use crate::registry::NullifierRegistry;
        let mut tx = make_shielded_tx();
        tx.merkle_root = [0u8; 32];
        let mut state = MerkleRootState::new(10);
        for i in 1..=11u64 {
            state.push(dummy_root(i)); // tip at height 11, genesis root pruned
        }
        let spent = NullifierRegistry::new();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::StaleMerkleRoot),
            "a root K+1 commits behind the tip (pruned from the bounded state) must be rejected stale"
        );
    }

    /// S4.3.3 seam: the parent item's headline discriminator, verbatim
    /// intent — "set K=0 in a test → the K-1 case now rejects (proves the
    /// window logic, not just equality)". The tx root (the genesis seed)
    /// WAS committed and IS in the history; it is rejected purely because
    /// the window shrank to 0. Containment/`==` logic alone cannot explain
    /// the outcome.
    #[test]
    fn check3_concrete_root_state_window_zero_rejects_one_back() {
        use crate::registry::NullifierRegistry;
        let mut tx = make_shielded_tx();
        tx.merkle_root = [0u8; 32];
        let mut state = MerkleRootState::new(0);
        state.push(dummy_root(1)); // tip at height 1 → the genesis root is 1 back
        let spent = NullifierRegistry::new();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 0 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::StaleMerkleRoot),
            "K=0 must reject a root even when it is in the committed history (window logic, not equality)"
        );
    }

    /// S4.3.3 seam: the accept half of K=0. No pushes — the genesis zero
    /// root IS the tip (distance 0 ≤ 0) → passes check 3, reaches check 4.
    /// Together with the previous test, pins "K=0 means exact tip only"
    /// from both sides.
    #[test]
    fn check3_concrete_root_state_window_zero_accepts_exact_tip() {
        use crate::registry::NullifierRegistry;
        let mut tx = make_shielded_tx();
        tx.merkle_root = [0u8; 32];
        let state = MerkleRootState::new(0); // genesis seed is the tip
        let spent = NullifierRegistry::new();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 0 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
            "K=0 must accept the exact tip"
        );
    }

    /// S4.3.3 seam: fail-closed on *unknown*, distinct from *stale*. The
    /// tx references a root that is a perfectly valid field element but was
    /// never committed — check 3 must reject it (a "accept if it looks
    /// like a valid field element" implementation fails here).
    #[test]
    fn check3_concrete_root_state_unknown_root_rejected() {
        use crate::registry::NullifierRegistry;
        let mut tx = make_shielded_tx();
        tx.merkle_root = dummy_root(999); // decodable, never committed
        let mut state = MerkleRootState::new(10);
        state.push(dummy_root(1));
        let spent = NullifierRegistry::new();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::StaleMerkleRoot),
            "a never-committed (unknown) root must be rejected, not silently accepted"
        );
    }

    /// S4.3.3 seam: the boundedness assertion seen through the spec.
    /// Window 2: push the tx root, then 3 dummies → the retained 3 entries
    /// drop the tx root → `StaleMerkleRoot`, AND `state.len() == 3` in the
    /// same test — the retention bound and its rejection consequence are
    /// asserted together, so the orphan-buffer analogy is proven at the
    /// validation boundary, not just at the type.
    #[test]
    fn check3_concrete_root_state_pruned_root_rejected() {
        use crate::registry::NullifierRegistry;
        let mut tx = make_shielded_tx();
        tx.merkle_root = dummy_root(1); // the tx root, committed at height 1
        let mut state = MerkleRootState::new(2);
        state.push(tx.merkle_root);
        state.push(dummy_root(2));
        state.push(dummy_root(3));
        state.push(dummy_root(4)); // retained: heights 2,3,4 — tx root gone
        let spent = NullifierRegistry::new();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 2 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::StaleMerkleRoot),
            "a root pruned from the bounded history must be rejected stale"
        );
        assert_eq!(state.len(), 3, "retention stays bounded to window + 1");
    }

    /// S4.3.4 — Decision 2's property end-to-end: the first transfer on a
    /// fresh network. The tx references the empty pool's root (`[0u8; 32]`,
    /// the genesis seed) and the state is `new(10)` with zero pushes — the
    /// genesis snapshot alone must satisfy check 3.
    ///
    /// **Discriminator:** a state constructed *without* the genesis seed
    /// (the one-line revert of Decision 2) is empty, so `is_root_fresh`
    /// takes the empty-history arm and this becomes `StaleMerkleRoot` —
    /// the seed is proven necessary, not cosmetic. No bootstrap deadlock.
    #[test]
    fn genesis_pool_state_accepts_first_transfer() {
        use crate::registry::NullifierRegistry;
        let mut tx = make_shielded_tx();
        tx.merkle_root = [0u8; 32]; // the empty pool root a wallet proves against pre-commits
        let state = MerkleRootState::new(10); // fresh network: genesis seed only, no pushes
        let spent = NullifierRegistry::new();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
        assert!(
            reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
            "the first transfer (referencing the genesis pool root) must pass check 3 and reach the proof check"
        );
    }

    #[test]
    fn validator_verifier_is_built_at_k10() {
        // S4.1.4 discriminator: the verifying key must be built at K=10 to match the
        // prover (S3.3 proves at K=10). A different width rejects every proof.
        assert_eq!(SHIELDED_VALIDATOR_VERIFIER.width(), 10);
    }

    #[test]
    fn checks_run_in_structural_then_nullifier_order() {
        // S4.1.6 discriminator: a tx that is BOTH structurally invalid (empty
        // nullifier vec) AND would fail the nullifier check must fail at check 1
        // (InvalidCommitment), not check 2 — proving checks run in order 1→2→3→4.
        let mut tx = make_shielded_tx();
        tx.nullifiers = vec![]; // check 1 structural failure
        let spent = FakeNullifier { spent: vec![[0u8; 32]] }; // would also fail check 2
        let roots = FakeRootHistory::default();
        let result = run_shielded(&tx, &spent, &roots, 10);
        assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
            "the structural failure must win over the nullifier check (order 1 before 2)");
    }

    #[test]
    fn risk_gate_rejects_shielded_above_max_risk() {
        // The shielded spec's neutral risk (0.30) is still gated by max_risk. With a
        // max_risk of 0.20 the neutral risk is rejected — same code path as the plain
        // spec tests above — proving the neutral risk flows through the gate.
        let tx = make_shielded_tx();
        let spent = FakeNullifier::default();
        let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
        let mut env = make_env_with_defaults();
        env.max_risk = 0.20;
        let deps = ShieldedValidationDeps { spent: &spent, roots: &roots, recency_window: 10 };
        let result = ShieldedValidationSpec::new().validate_shielded(&tx, &env, &deps);
        assert!(reason_matches(&result, ValidationFailureReason::RiskExceedsThreshold),
            "the neutral risk must still be rejected by the max_risk gate below its score");
    }

    // --- S4.1.6 end-to-end live prove (benchmark-only, #[ignore]d) ---

    /// The ONE end-to-end live proof through `validate_shielded`. `#[ignore]`d per
    /// the roadmap's "proving is benchmark-only" rule; run on demand with:
    ///
    /// ```text
    /// cargo test --workspace -p pneumatic_core -- --ignored validate_shielded_end_to_end_live_prove
    /// ```
    ///
    /// Produces a real Halo2 proof over a satisfiable Action circuit, assembles a
    /// `ShieldedTransaction` whose reconstructed public inputs equal the circuit's,
    /// then asserts `validate_shielded` returns `Ok` through all four checks, and
    /// that flipping the proof yields `InvalidShieldedProof`. This proves checks 1-4
    /// all pass together on a real proof and that check 4 is genuine (not vacuous).
    #[test]
    #[ignore]
    fn validate_shielded_end_to_end_live_prove() {
        use halo2_proofs::pasta::EqAffine;
        use halo2_proofs::plonk::{create_proof, keygen_pk, keygen_vk};
        use halo2_proofs::poly::commitment::Params;
        use halo2_proofs::transcript::{Blake2bWrite, Challenge255};
        use rand::rngs::OsRng;

        // A satisfiable 1-in/1-out circuit with a matching tx fixture (fee 100 = 90 + 10).
        let note = ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) };
        let spend_key = [0xABu8; 32];
        let output_note = ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3), rcm: Fq::from(4) };
        let fee: u64 = 10;
        let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        let (merkle_root, merkle_proof) = tree.append(&commit(&note));
        let circuit = ActionCircuit::new(note.clone(), spend_key, merkle_proof, merkle_root, output_note.clone(), fee, DEFAULT_DEPTH);
        let public_inputs = circuit.public_inputs();

        // Prove it at K = 10 (must match the verifying key's width).
        let params: Params<EqAffine> = Params::new(10);
        let vk = keygen_vk(&params, &circuit).expect("keygen_vk");
        let pk = keygen_pk(&params, vk, &circuit).expect("keygen_pk");
        let instances_owned = ShieldedVerifier::instances_for(&public_inputs);
        let columns_per_proof: Vec<Vec<&[Fp]>> = instances_owned.iter().map(|cols| cols.iter().map(|c| c.as_slice()).collect::<Vec<&[Fp]>>()).collect();
        let proofs: Vec<&[&[Fp]]> = columns_per_proof.iter().map(|cols| cols.as_slice()).collect();
        let instances: &[&[&[Fp]]] = &proofs;
        let mut transcript = Blake2bWrite::<_, EqAffine, Challenge255<EqAffine>>::init(Vec::new());
        create_proof(&params, &pk, &[circuit.clone()], instances, &mut OsRng, &mut transcript).expect("create_proof on a satisfiable circuit");
        let proof = transcript.finalize();

        // Assemble the wire tx with the real proof and matching fee.
        let tx = make_shielded_tx_with(proof, fee);

        let spent = FakeNullifier { spent: vec![] };
        let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
        let env = make_env_with_defaults();
        let deps = ShieldedValidationDeps { spent: &spent, roots: &roots, recency_window: 10 };

        // (a) A real proof over a fresh nullifier and current root validates through
        //     all four checks.
        assert!(
            matches!(ShieldedValidationSpec::new().validate_shielded(&tx, &env, &deps), Ok(_)),
            "a real proof must validate through all four checks"
        );

        // (b) Flipping one proof byte makes check 4 reject — proving check 4 is
        //     genuine, not vacuous.
        let mut bad = tx.clone();
        if let Some(last) = bad.proof.last_mut() {
            *last ^= 0xff;
        }
        let bad_result = ShieldedValidationSpec::new().validate_shielded(&bad, &env, &deps);
        assert!(reason_matches(&bad_result, ValidationFailureReason::InvalidShieldedProof),
            "a tampered proof must be rejected as InvalidShieldedProof");
    }
}
