use std::collections::HashMap;
use std::sync::Arc;
use serde::Serialize;
use crate::data::DataProvider;
use crate::environment::EnvironmentMetadata;
use crate::errors::{ValidationFailureReason, TransactionRiskFactor, PneumaticError, ReconciledSignatures};
use crate::tokens::Token;
use crate::transactions::{Transaction, TransactionValidationResult};

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
        EnvironmentMetadata::load_from_spec(spec)
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
            gen_block.current_hash = BlockFactory::create_hash(&gen_block);
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
        block.current_hash = BlockFactory::create_hash(&block);
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
}
