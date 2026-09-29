//! ADR-015, Phase 6: the executor's `DeployContract` protocol op end-to-end.
//!
//! A `DeployContract` transaction is dispatched to `execute_deploy` (not the
//! contract path — it has no target token and no engine to select). The
//! executor emits a canonical `CreateTokenDelta` in `result_data`, hashes it,
//! and votes to the finalizer over that hash. This test drives a full
//! `run_execution` and asserts the deterministic token id, the result hash,
//! and the gas model.

use super::helpers::*;
use super::super::*;

use pneumatic_core::contracts::{deploy_contract, deploy_gas, DeployParams};
use pneumatic_core::crypto::HashProvider;
use pneumatic_core::data::StubDataProvider;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::transactions::{PendingTransaction, Transaction, TransactionState};
use std::collections::HashMap;
use std::sync::Arc;

/// Poll until the spawned task has settled (its backpressure slot is freed),
/// then read the recorded outcome from the per-tx results map. Returns `None`
/// on timeout. (Local copy — the identical helper in `dispatch.rs` is private.)
async fn wait_for_settle(
    executor: &Executor,
    tx_id: &str,
) -> Option<Result<ExecutionResult, String>> {
    for _ in 0..500 {
        let active = executor.active_tasks.lock().await;
        let settled = !active.contains(tx_id);
        drop(active);
        if settled {
            let tasks = executor.preload_tasks.lock().await;
            return tasks
                .get(tx_id)
                .and_then(|results| results.get(tx_id).map(|r| r.clone()));
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    None
}

/// Build a `DeployContract` pending-registry entry.
fn deploy_entry(
    params: DeployParams,
    sender: Vec<u8>,
    nonce: usize,
) -> (PendingTransaction, DeployParams, Vec<u8>) {
    let tx = Transaction {
        payload: serialize_to_bytes_rmp(&params).unwrap(),
        gas_limit: 0,
        id: "deploy_tx_001".to_string(),
        action: "DeployContract".to_string(),
        token_id: vec![],
        bid: None,
        sequence_number: nonce,
        sender: sender.clone(),
        receiver: vec![],
        amount: None,
        timestamp: 1000,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let pending =
        PendingTransaction::new("deploy_tx_001".to_string(), TransactionState::Preloaded {
            transaction: tx,
        });
    (pending, params, sender)
}

#[tokio::test]
async fn deploy_produces_deterministic_token_and_result_hash() {
    let identity = Arc::new(NodeIdentity::generate_in_memory());
    let node_registry = make_test_node_registry();
    let finalizer_key = vec![0xAB; 32];
    let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
    node_registry.register_peer(
        finalizer_key.clone(),
        [5u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(RecordingConnection {
            recorder: recorder.clone(),
        }),
    );

    let params = DeployParams {
        name: "mycontract".to_string(),
        engine: "Spec".to_string(),
        bytecode: vec![9, 8, 7, 6, 5, 4, 3, 2, 1],
        metadata: HashMap::new(),
    };
    let (pending, params, sender) = deploy_entry(params, vec![0x42; 32], 7);

    let pending_registry = Arc::new(pneumatic_core::registry::PendingTransactionRegistry::new());
    pending_registry
        .add_transaction("deploy_tx_001".to_string(), pending)
        .unwrap();

    // The deploy path does not fetch a token or user, so an empty data
    // provider is sufficient.
    let data_provider = Arc::new(StubDataProvider::new());
    let hash_provider = make_test_hash_provider();

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        identity.clone(),
        node_registry,
        data_provider,
        pending_registry,
        hash_provider.clone(),
        100,
        "token".to_string(),
        make_test_engine_registry(),
    );

    executor
        .preload_for_transaction("deploy_tx_001")
        .await
        .expect("preload should be admitted");

    let outcome = wait_for_settle(&executor, "deploy_tx_001")
        .await
        .expect("task should settle");
    let result = outcome.expect("deploy execution should succeed");

    // Re-derive the expected delta (same formula as the executor) and assert
    // the result bytes and hash match.
    let hash = pneumatic_core::crypto::BasicHashProvider::new();
    let expected_delta = deploy_contract(&sender, 7, &params, &hash).unwrap();
    let expected_bytes = serialize_to_bytes_rmp(&expected_delta).unwrap();
    let expected_hash = hash_provider.hash(&expected_bytes);

    assert_eq!(result.result_data, expected_bytes);
    assert_eq!(result.result_hash, expected_hash);
    // Gas model: base + per-byte (QD6).
    assert_eq!(result.gas_used, deploy_gas(params.bytecode.len()));
}
