//! ADR-018, Phase 7: the committer's idempotent Wasm `StorageDelta` apply path.
//!
//! Unlike the re-derivable `DeployContract` delta, a Wasm storage delta is the
//! module's `sstore` output and cannot be recomputed from the transaction. The
//! executor ships the canonical `WasmResult` envelope in `Transaction::result_data`;
//! the committer verifies it against the signed `result_hash` and applies the
//! delta to the contract token's `SmartContract::storage`. The apply is a no-op
//! for a missing token, a non-Wasm contract, or an empty `result_data`, and is
//! idempotent under replay (BTreeMap set/remove).

use super::helpers::*;
use pneumatic_core::contracts::{StorageDelta, WasmResult};
use pneumatic_core::crypto::HashProvider;
use pneumatic_core::tokens::{SmartContract, Token};
use std::collections::BTreeMap;
use std::sync::Arc;

/// QD4: the storage apply writes the token under `partition_id == environment_id`
/// (the committer test environment id is `"test"`).
const PARTITION: &str = "test";

/// Build a Wasm contract token (metadata `contract_engine == "Wasm"`, a
/// `SmartContract` asset with the given initial storage).
fn wasm_contract_token(token_id: Vec<u8>, initial_storage: BTreeMap<Vec<u8>, Vec<u8>>) -> Token {
    let mut token = Token::new();
    token.id = token_id;
    token.environment_id = "test".to_string();
    token
        .metadata
        .insert("contract_engine".to_string(), "Wasm".to_string());
    token
        .metadata
        .insert("token_type".to_string(), "contract".to_string());
    let sc = SmartContract {
        name: "wasmcontract".to_string(),
        bytecode: vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00],
        version: "1".to_string(),
        storage: initial_storage,
    };
    token.set_asset(&sc).unwrap();
    token
}

/// Build a `ContractCall` transaction whose `result_data` is a canonical
/// `WasmResult` envelope and `result_hash` is its hash (so the integrity check
/// in `apply_storage_delta` passes).
fn make_contract_call_tx(token_id: Vec<u8>, module_output: Vec<u8>, delta: StorageDelta) -> Transaction {
    let hash = BasicHashProvider::new();
    let wasm_result = WasmResult {
        module_output,
        storage_delta: delta,
    };
    let result_data = serialize_to_bytes_rmp(&wasm_result).unwrap();
    let result_hash = hash.hash(&result_data);
    Transaction {
        payload: vec![],
        gas_limit: 0,
        id: "contract_call_tx".to_string(),
        action: "ContractCall".to_string(),
        token_id,
        bid: None,
        sequence_number: 0,
        sender: vec![0x42; 32],
        receiver: vec![],
        amount: None,
        timestamp: 0,
        result_hash,
        sender_signature: vec![],
        result_data,
    }
}

fn delta_with(k: &[u8], v: Option<Vec<u8>>) -> StorageDelta {
    let mut d: StorageDelta = BTreeMap::new();
    d.insert(k.to_vec(), v);
    d
}

/// A set delta is applied to the contract's storage.
#[test]
fn storage_delta_applies_to_contract_token() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![7, 8, 9];
    dp.insert_token(token_id.clone(), PARTITION.to_string(), wasm_contract_token(token_id.clone(), BTreeMap::new()));

    let tx = make_contract_call_tx(token_id.clone(), vec![1, 2, 3], delta_with(b"counter", Some(b"42".to_vec())));
    committer.apply_storage_delta(&tx).expect("storage delta should apply");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    assert_eq!(sc.storage.get(b"counter".to_vec().as_slice()), Some(&b"42".to_vec()));
}

/// A tombstone (None) delta removes the key from the contract's storage.
#[test]
fn storage_delta_tombstone_removes_key() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![10, 11, 12];
    let mut initial = BTreeMap::new();
    initial.insert(b"to_delete".to_vec(), b"gone".to_vec());
    dp.insert_token(token_id.clone(), PARTITION.to_string(), wasm_contract_token(token_id.clone(), initial));

    let tx = make_contract_call_tx(token_id.clone(), vec![], delta_with(b"to_delete", None));
    committer.apply_storage_delta(&tx).expect("tombstone should apply");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    assert!(sc.storage.get(b"to_delete".to_vec().as_slice()).is_none());
}

/// Re-applying the same delta is a no-op (idempotent), not an error.
#[test]
fn storage_delta_is_idempotent_on_replay() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![13, 14, 15];
    dp.insert_token(token_id.clone(), PARTITION.to_string(), wasm_contract_token(token_id.clone(), BTreeMap::new()));

    let tx = make_contract_call_tx(token_id.clone(), vec![], delta_with(b"k", Some(b"v".to_vec())));
    committer.apply_storage_delta(&tx).expect("first apply");
    committer.apply_storage_delta(&tx).expect("replay is a no-op");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    assert_eq!(sc.storage.get(b"k".to_vec().as_slice()), Some(&b"v".to_vec()));
}

/// A tampered `result_hash` fails closed (integrity check).
#[test]
fn storage_delta_rejects_tampered_result_hash() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![16, 17, 18];
    dp.insert_token(token_id.clone(), PARTITION.to_string(), wasm_contract_token(token_id.clone(), BTreeMap::new()));

    let mut tx = make_contract_call_tx(token_id.clone(), vec![], delta_with(b"k", Some(b"v".to_vec())));
    tx.result_hash = vec![0xFF; 32];
    let err = committer
        .apply_storage_delta(&tx)
        .expect_err("tampered hash must be rejected");
    assert!(matches!(err, CommitterError::TransactionPayloadMismatch(_)));
}

/// A non-Wasm contract token is a no-op (its result_data is not a WasmResult
/// envelope), and its storage is untouched.
#[test]
fn storage_delta_noop_for_non_wasm_token() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![19, 20, 21];
    // A "Spec" (non-Wasm) contract token.
    let mut token = wasm_contract_token(token_id.clone(), BTreeMap::new());
    token.metadata.insert("contract_engine".to_string(), "Spec".to_string());
    dp.insert_token(token_id.clone(), PARTITION.to_string(), token);

    // A well-formed WasmResult envelope, but the token is not Wasm → no-op.
    let tx = make_contract_call_tx(token_id.clone(), vec![], delta_with(b"k", Some(b"v".to_vec())));
    committer.apply_storage_delta(&tx).expect("non-wasm is a no-op");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    assert!(sc.storage.is_empty(), "non-Wasm token storage must be untouched");
}

/// A missing token is a no-op (nothing to apply).
#[test]
fn storage_delta_noop_for_missing_token() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    // No token inserted.
    let tx = make_contract_call_tx(vec![99, 99], vec![], delta_with(b"k", Some(b"v".to_vec())));
    committer.apply_storage_delta(&tx).expect("missing token is a no-op");
}

/// An empty `result_data` (legacy call) is a no-op.
#[test]
fn storage_delta_noop_for_empty_result_data() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![22, 23, 24];
    dp.insert_token(token_id.clone(), PARTITION.to_string(), wasm_contract_token(token_id.clone(), BTreeMap::new()));

    let mut tx = make_contract_call_tx(token_id.clone(), vec![], delta_with(b"k", Some(b"v".to_vec())));
    tx.result_data = vec![]; // strip the envelope
    committer.apply_storage_delta(&tx).expect("empty result_data is a no-op");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    assert!(sc.storage.is_empty());
}
