//! Shared fixtures for the validation test suite (committer convention:
//! cross-file fixtures live here, pub-ified, re-exporting the module
//! header's test-only types).

use super::super::*;
pub use crate::environment::EnvironmentMetadataSpec;
pub use crate::transactions::{PendingTransaction, TransactionState};
pub use crate::registry::PendingTransactionRegistry;
pub use crate::rns::identity::NodeIdentity;
pub use crate::crypto::AsymCryptoProvider;
// Phase S4.1 shielded-validation tests: the note/tree primitives for building
// well-formed `ShieldedTransaction` fixtures, and the scalar field for them.
pub use crate::shielded::{
    ActionCircuit, commit, nullifier, root_to_bytes, ShieldedNote, IncrementalMerkleTree, DEFAULT_DEPTH,
};
pub use group::GroupEncoding;
pub use pasta_curves::pallas::Scalar as Fq;

// --- helpers ---


pub fn make_token_with_owner(owner: &[u8]) -> Token {
    let mut token = Token::new();
    // Owner is stored as hex (AUDIT 5.9): the tx-level SelfSigned spec decodes
    // it with hex::decode. Using String::from_utf8 here would break the round
    // trip for any non-UTF-8 key.
    token.set_metadata("owner".to_string(), hex::encode(owner));
    token
}

pub fn make_env_with_defaults() -> EnvironmentMetadata {
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

pub fn make_tx(sender: &[u8], receiver: &[u8], amount: Option<u64>, seq: usize) -> Transaction {
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

// --- Test helpers for block-level validators ---

pub use crate::blocks::{BlockFactory, Blockchain};
pub use crate::transactions::{SignedTransaction, TransactionSignature};
pub use std::collections::HashMap;

pub fn make_signed_tx_with_fields(
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

pub fn make_valid_block(signed_tx: SignedTransaction, blockchain: &mut Blockchain) -> crate::blocks::Block {
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

pub fn make_self_signed_token(owner: &[u8]) -> Token {
    let mut token = Token::new();
    token.set_metadata("owner".to_string(), hex::encode(owner));
    token.is_self_verified = true;
    token.block_validation_spec_name = String::from("SelfSigned");
    token
}

// -----------------------------------------------------------------------
// Phase S4.1 — ShieldedValidationSpec (four fail-closed checks)
// -----------------------------------------------------------------------

/// Nullifier-set fake implementing the `NullifierMembership` interface so
/// check 2 can run against in-memory state (S4.1.5) without the S4.2 type.
#[derive(Default)]
pub struct FakeNullifier {
    pub spent: Vec<[u8; 32]>,
}
impl NullifierMembership for FakeNullifier {
    fn contains_nullifier(&self, nullifier: [u8; 32]) -> bool {
        self.spent.iter().any(|n| *n == nullifier)
    }
}

/// Root-history fake implementing the `MerkleRootHistory` interface so check 3
/// can walk a known history (S4.1.5) without the S4.3 type.
#[derive(Default)]
pub struct FakeRootHistory {
    pub roots: Vec<RootSnapshot>,
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
pub fn make_shielded_tx() -> ShieldedTransaction {
    make_shielded_tx_with(vec![0u8; 64], 0)
}

/// Same fixture with an explicit proof buffer and fee. The fee must match the
/// value the verifier's proof is built over (check 4's `public_inputs.fee`).
pub fn make_shielded_tx_with(proof: Vec<u8>, fee: u64) -> ShieldedTransaction {
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
pub fn run_shielded(
    tx: &ShieldedTransaction,
    spent: &FakeNullifier,
    roots: &FakeRootHistory,
    window: usize,
) -> Result<TransactionValidationResult, PneumaticError> {
    let deps = ShieldedValidationDeps { spent, roots, recency_window: window };
    ShieldedValidationSpec::new().validate_shielded(tx, &make_env_with_defaults(), &deps)
}

pub fn reason_matches(result: &Result<TransactionValidationResult, PneumaticError>, r: ValidationFailureReason) -> bool {
    matches!(
        result,
        Err(PneumaticError::Validation(ref rs)) if rs.iter().any(|reason| *reason == r)
    )
}

// -----------------------------------------------------------------------
// Phase S4.3 — S4.1's check-3 discriminators re-run against the concrete
// `MerkleRootState` (S4.3.3). The real `NullifierRegistry` backs
// `deps.spent` (fresh, so check 2 passes) and deps are built inline:
// the `run_shielded` helper above is typed to the S4.1 fakes, which stay
// in place to pin the window arithmetic on a full-history view.
// -----------------------------------------------------------------------

pub use crate::shielded::MerkleRootState;
use pasta_curves::pallas::Base as Fp;

/// Distinct, decodable dummy pool roots: `Fp::from(tag)` in canonical
/// form, so `public_inputs_from_shielded_tx` decodes them. Check 3
/// compares raw bytes, so the dummies never need to correspond to a
/// real tree (they must, however, be valid field elements).
pub fn dummy_root(tag: u64) -> [u8; 32] {
    Fp::from(tag).to_repr()
}
