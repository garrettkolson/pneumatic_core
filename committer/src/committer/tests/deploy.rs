//! ADR-015, Phase 6: the committer's idempotent `CreateTokenDelta` apply path.
//!
//! The executor emits a canonical `CreateTokenDelta` in `result_data` and the
//! finalizer signs its hash. The committer does not receive the raw bytes, but
//! the delta is a pure function of the transaction (`sender`, `nonce`,
//! `DeployParams`, `hash`) — so `apply_deploy_delta` re-derives it, verifies it
//! against the signed `result_hash`, and creates the token idempotently: the
//! token is created once, and a replayed deploy is a no-op.

use super::helpers::*;
use pneumatic_core::contracts::{deploy_contract, DeployParams};
use pneumatic_core::crypto::HashProvider;
use std::collections::HashMap;
use std::sync::Arc;

/// QD4: the deploy apply writes the token under `partition_id ==
/// environment_id`. The committer test environment id is `"test"`.
const PARTITION: &str = "test";

/// Build a `DeployContract` transaction whose `result_hash` is the canonical
/// hash of the delta the committer will re-derive (so the integrity check in
/// `apply_deploy_delta` passes).
fn make_deploy_tx(sender: Vec<u8>, nonce: usize, params: DeployParams) -> Transaction {
    let hash = BasicHashProvider::new();
    let delta = deploy_contract(&sender, nonce as u64, &params, &hash).unwrap();
    let result_bytes = serialize_to_bytes_rmp(&delta).unwrap();
    let result_hash = hash.hash(&result_bytes);
    Transaction {
        payload: serialize_to_bytes_rmp(&params).unwrap(),
        gas_limit: 0,
        id: "deploy_tx".to_string(),
        action: "DeployContract".to_string(),
        token_id: vec![],
        bid: None,
        sequence_number: nonce,
        sender,
        receiver: vec![],
        amount: None,
        timestamp: 0,
        result_hash,
        sender_signature: vec![],
    
    result_data: vec![],}
}

fn spec_params() -> DeployParams {
    DeployParams {
        name: "mycontract".to_string(),
        engine: "Spec".to_string(),
        bytecode: vec![1, 2, 3, 4, 5],
        metadata: HashMap::new(),
    }
}

#[test]
fn deploy_delta_creates_token_in_env_partition() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());

    let params = spec_params();
    let sender = vec![0x42; 32];
    let tx = make_deploy_tx(sender.clone(), 0, params.clone());

    committer.apply_deploy_delta(&tx).expect("deploy should apply");

    // Re-derive the expected token id to assert the token landed in the
    // environment partition with the contract metadata.
    let hash = BasicHashProvider::new();
    let delta = deploy_contract(&sender, 0, &params, &hash).unwrap();
    let token = dp.get_token(&delta.token_id, PARTITION).expect("token should exist");
    assert_eq!(token.id, delta.token_id);
    assert_eq!(token.environment_id, "test");
    assert_eq!(
        token.metadata.get("token_type").map(|s| s.as_str()),
        Some("contract")
    );
    assert_eq!(
        token.metadata.get("contract_engine").map(|s| s.as_str()),
        Some("Spec")
    );
}

#[test]
fn deploy_delta_is_idempotent_on_replay() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());

    let params = spec_params();
    let sender = vec![0x42; 32];
    let tx = make_deploy_tx(sender.clone(), 0, params.clone());

    committer.apply_deploy_delta(&tx).expect("first apply");
    // A replayed deploy must be a no-op, not an error.
    committer.apply_deploy_delta(&tx).expect("replay is a no-op");

    let hash = BasicHashProvider::new();
    let delta = deploy_contract(&sender, 0, &params, &hash).unwrap();
    let token = dp.get_token(&delta.token_id, PARTITION).expect("token should exist");
    assert_eq!(token.id, delta.token_id);
}

#[test]
fn deploy_delta_rejects_tampered_result_hash() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());

    let params = spec_params();
    let sender = vec![0x42; 32];
    let mut tx = make_deploy_tx(sender.clone(), 0, params.clone());
    // Tamper with the result hash — the integrity check must fail closed.
    tx.result_hash = vec![0xFF; 32];

    let err = committer
        .apply_deploy_delta(&tx)
        .expect_err("tampered hash must be rejected");
    assert!(matches!(
        err,
        CommitterError::TransactionPayloadMismatch(_)
    ));
}
