//! Phase 3: real `execute_contract` dispatch in the executor.
//!
//! Covers the plan's Phase 3 exit criteria:
//! - the identity stub is gone — a contract tx's block carries a *computed*
//!   `result_hash` (the SHA-256 of the engine's canonical output, not the tx
//!   bytes);
//! - failure paths transition the tx to `Failed` with reasons (observable via
//!   the per-tx results map, which now records `Err` as well as `Ok`);
//! - defect D4: the backpressure slot is freed on settle, so N sequential
//!   preloads at `max_in_flight = 1` never hit `AtCapacity`;
//! - defect D3: data is fetched under the token partition id, not the env id.
use super::helpers::*;
use super::super::*;

use pneumatic_core::data::StubDataProvider;
use pneumatic_core::messages::Message;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::tokens::{SmartContract, Token};
use pneumatic_core::transactions::TransactionSignature;
use pneumatic_core::user::User;

/// Poll until the spawned task has settled (its backpressure slot is freed),
/// then read the recorded outcome from the per-tx results map. Returns
/// `None` on timeout.
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

/// A data provider holding a token (with a `SmartContract` asset) and a sender
/// user, both under the `"token"` partition — the shape `run_execution`
/// expects for a full successful dispatch.
fn contract_data_provider(
    token_id: Vec<u8>,
    sender: Vec<u8>,
) -> Arc<dyn DataProvider> {
    let contract = SmartContract {
        name: "test".to_string(),
        bytecode: vec![1, 2, 3],
        version: "1".to_string(),
        storage: Default::default(),
    };
    let token = Token::from_asset(&contract).unwrap().with_id(token_id.clone());
    let user = User::new(sender.clone());
    Arc::new(
        StubDataProvider::new()
            .with_token(token_id, "token".to_string(), token)
            .with_user(sender, "token".to_string(), user),
    )
}

// --- Dispatch happy path: computed result_hash + Sign vote over it ---

#[tokio::test]
async fn dispatch_happy_path_produces_result_hash_and_sign_vote() {
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

    // `make_test_pending_registry` holds `test_tx_001` (token [0,1,2],
    // sender [10,20,30], receiver [40,50,60], amount 100, seq 1).
    let pending_registry = make_test_pending_registry();
    let data_provider = contract_data_provider(vec![0, 1, 2], vec![10, 20, 30]);
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
        .preload_for_transaction("test_tx_001")
        .await
        .expect("preload should be admitted");

    let outcome = wait_for_settle(&executor, "test_tx_001")
        .await
        .expect("task should settle");
    let result = outcome.expect("execution should succeed");

    // The engine output is the canonical transfer delta; the result hash is
    // its SHA-256 (NOT a hash of the tx bytes — that was the identity stub).
    let expected_delta = pneumatic_core::contracts::TransferDelta {
        token_id: vec![0, 1, 2],
        sender: vec![10, 20, 30],
        receiver: vec![40, 50, 60],
        amount: 100,
        sequence_number: 1,
    };
    let expected_bytes = serialize_to_bytes_rmp(&expected_delta).unwrap();
    let expected_hash = hash_provider.hash(&expected_bytes);

    assert_eq!(result.result_data, expected_bytes);
    assert_eq!(result.result_hash, expected_hash);
    assert_eq!(result.gas_used, pneumatic_core::contracts::TRANSFER_BASE_COST);

    // The "Sign" vote carries the engine-output hash.
    let captured = recorder.lock().unwrap();
    let sign = captured
        .iter()
        .map(|b| deserialize_rmp_to::<Message>(b).unwrap())
        .find(|m| m.action == "Sign")
        .expect("a Sign message should be sent to the finalizer");
    let vote: TransactionSignature = deserialize_rmp_to(&sign.body).unwrap();
    assert_eq!(vote.transaction_hash, expected_hash);
}

// --- Failure path: missing contract asset -> Failed with reasons ---

#[tokio::test]
async fn dispatch_failure_records_err_and_transitions_to_failed() {
    let identity = Arc::new(NodeIdentity::generate_in_memory());
    let node_registry = make_test_node_registry();
    let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
    node_registry.register_peer(
        vec![0xAB; 32],
        [5u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(RecordingConnection {
            recorder: recorder.clone(),
        }),
    );

    let pending_registry = make_test_pending_registry();
    // A token with NO contract asset -> `execute_contract` fails closed with
    // `ContractNotFound`.
    let bare_token = Token::from_asset(&"not-a-contract").unwrap().with_id(vec![0, 1, 2]);
    let data_provider: Arc<dyn DataProvider> = Arc::new(
        StubDataProvider::new()
            .with_token(vec![0, 1, 2], "token".to_string(), bare_token)
            .with_user(vec![10, 20, 30], "token".to_string(), User::new(vec![10, 20, 30])),
    );

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        identity.clone(),
        node_registry,
        data_provider,
        pending_registry.clone(),
        make_test_hash_provider(),
        100,
        "token".to_string(),
        make_test_engine_registry(),
    );

    executor
        .preload_for_transaction("test_tx_001")
        .await
        .expect("preload should be admitted");

    let outcome = wait_for_settle(&executor, "test_tx_001")
        .await
        .expect("task should settle");
    let err = outcome.expect_err("execution should fail");
    assert!(err.contains("ContractNotFound"), "got: {}", err);

    // The tx transitioned to Failed in the pending registry.
    let entry = pending_registry
        .get_transaction_mut("test_tx_001")
        .expect("tx still in registry");
    assert!(
        matches!(entry.state, TransactionState::Failed { .. }),
        "tx should be in a Failed state, got {:?}",
        entry.state
    );
}

// --- Defect D4: the backpressure slot is freed on settle ---

#[tokio::test]
async fn backpressure_slot_freed_after_settle() {
    // A plain data provider: unknown token -> `run_execution` fails fast with
    // a Data error, so each spawned task settles (and frees its slot) quickly.
    let data_provider = make_test_data_provider();
    let node_registry = make_test_node_registry();
    let pending_registry = Arc::new(PendingTransactionRegistry::new());

    // Four distinct transactions, each in Preloaded state.
    for i in 0..4usize {
        let tx_id = format!("d4_tx_{}", i);
        let tx = Transaction {
            payload: vec![],
            gas_limit: 0,
            id: tx_id.clone(),
            action: "Transfer".to_string(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: vec![],
            receiver: vec![],
            amount: Some(0),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        
        result_data: vec![],};
        let pending =
            PendingTransaction::new(tx_id.clone(), TransactionState::Preloaded { transaction: tx });
        let _ = pending_registry.add_transaction(tx_id, pending);
    }

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry,
        make_test_hash_provider(),
        1, // capacity of 1 — a leaked slot would block the next preload
        "token".to_string(),
        make_test_engine_registry(),
    );

    // Sequentially preload all four. With a leaked slot (the pre-D4 bug), the
    // second preload would hit `AtCapacity`. With the slot freed on settle,
    // each is admitted once the previous task has settled.
    for i in 0..4usize {
        let tx_id = format!("d4_tx_{}", i);
        // Wait for any in-flight task to settle before the next preload.
        while executor.in_flight_count().await > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let r = executor.preload_for_transaction(&tx_id).await;
        assert!(
            r.is_ok(),
            "preload {} should be admitted (slot freed on settle), got {:?}",
            i,
            r
        );
    }
}

// --- Defect D3: data is fetched under the token partition, not the env id ---

#[tokio::test]
async fn partition_id_used_for_fetch() {
    // The token lives under partition "token". If the executor fetched under
    // the env id ("test_env") instead, `get_token` would return DataNotFound
    // and the dispatch would fail. A successful dispatch proves the partition
    // key is used.
    let identity = Arc::new(NodeIdentity::generate_in_memory());
    let node_registry = make_test_node_registry();
    let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
    node_registry.register_peer(
        vec![0xAB; 32],
        [5u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(RecordingConnection {
            recorder: recorder.clone(),
        }),
    );

    let pending_registry = make_test_pending_registry();
    let data_provider = contract_data_provider(vec![0, 1, 2], vec![10, 20, 30]);
    let hash_provider = make_test_hash_provider();

    let executor = Executor::new(
        "test_env".to_string(), // env id != partition id
        vec![1, 2, 3, 4],
        identity.clone(),
        node_registry,
        data_provider,
        pending_registry,
        hash_provider.clone(),
        100,
        "token".to_string(), // partition id
        make_test_engine_registry(),
    );

    executor
        .preload_for_transaction("test_tx_001")
        .await
        .expect("preload should be admitted");

    let outcome = wait_for_settle(&executor, "test_tx_001")
        .await
        .expect("task should settle");
    let result = outcome.expect("dispatch should succeed via the partition key");
    assert!(!result.result_hash.is_empty());
}
