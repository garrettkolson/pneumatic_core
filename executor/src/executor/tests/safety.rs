//! Phase 4: safety hardening of the executor.
//!
//! Covers the plan's Phase 4 exit criteria ("no unbounded resource path in the
//! executor"):
//! - **Wall-clock backstop (Q3.2):** a stuck engine that exceeds the per-execution
//!   timeout fails the transaction (with `ExecutionTimeout`) instead of hanging the
//!   worker task, and its backpressure slot is freed.
//! - **Panic isolation:** a panicking engine fails the transaction (with
//!   `ContractExecutionFailed`) instead of unwinding the worker task.
//! - **Backpressure under timeout load:** N sequential timed-out executions at
//!   `max_in_flight = 1` never hit `AtCapacity` (the slot is freed on the
//!   timeout path too).
use super::helpers::*;
use super::super::*;

use std::time::Duration;

use pneumatic_core::contracts::{
    ContractEngine, ContractEngineRegistry, ContractError, ExecutionInput, ExecutionOutput,
};
use pneumatic_core::data::StubDataProvider;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::tokens::{SmartContract, Token};
use pneumatic_core::user::User;

// --- Test engines -----------------------------------------------------------

/// An engine that sleeps for a fixed duration — used to exercise the wall-clock
/// backstop (Q3.2). It runs on a blocking thread, so the sleep cannot block the
/// async worker that checks the timeout.
struct SlowEngine {
    sleep: Duration,
}

impl ContractEngine for SlowEngine {
    fn name(&self) -> &str {
        "Slow"
    }
    fn execute(&self, _input: &ExecutionInput) -> Result<ExecutionOutput, ContractError> {
        std::thread::sleep(self.sleep);
        Ok(ExecutionOutput::new(vec![1, 2, 3], 100))
    }
}

/// An engine that panics — used to exercise panic isolation (Phase 4).
struct BoomEngine;

impl ContractEngine for BoomEngine {
    fn name(&self) -> &str {
        "Boom"
    }
    fn execute(&self, _input: &ExecutionInput) -> Result<ExecutionOutput, ContractError> {
        panic!("deliberate engine panic (test)");
    }
}

/// A registry carrying the Tier-1 defaults plus a `SlowEngine`.
fn registry_with_slow(sleep: Duration) -> Arc<ContractEngineRegistry> {
    let registry = Arc::new(ContractEngineRegistry::new());
    registry.register_defaults();
    registry.register(Arc::new(SlowEngine { sleep }));
    registry
}

/// A registry carrying the Tier-1 defaults plus a panicking `BoomEngine`.
fn registry_with_boom() -> Arc<ContractEngineRegistry> {
    let registry = Arc::new(ContractEngineRegistry::new());
    registry.register_defaults();
    registry.register(Arc::new(BoomEngine));
    registry
}

/// A token that carries a `SmartContract` asset and selects the named engine
/// via the `contract_engine` metadata key (ADR-011), plus a sender user — both
/// under the `"token"` partition, the shape `run_execution` expects.
fn engine_data_provider(
    token_id: Vec<u8>,
    sender: Vec<u8>,
    engine_name: &str,
) -> Arc<dyn DataProvider> {
    let contract = SmartContract {
        name: "test".to_string(),
        bytecode: vec![1, 2, 3],
        version: "1".to_string(),
        storage: Default::default(),
    };
    let mut token = Token::from_asset(&contract).unwrap().with_id(token_id.clone());
    token
        .metadata
        .insert("token_type".to_string(), "contract".to_string());
    token
        .metadata
        .insert("contract_engine".to_string(), engine_name.to_string());
    let user = User::new(sender.clone());
    Arc::new(
        StubDataProvider::new()
            .with_token(token_id, "token".to_string(), token)
            .with_user(sender, "token".to_string(), user),
    )
}

/// Poll until the spawned task has settled (its backpressure slot is freed),
/// then read the recorded outcome from the per-tx results map. Returns `None`
/// on timeout.
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

// --- Wall-clock backstop (Q3.2): a stuck engine fails the tx, not the task ---

#[tokio::test]
async fn timeout_fails_tx_not_hang() {
    let node_registry = make_test_node_registry();
    let pending_registry = make_test_pending_registry(); // test_tx_001
    let data_provider = engine_data_provider(vec![0, 1, 2], vec![10, 20, 30], "Slow");

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry.clone(),
        make_test_hash_provider(),
        100,
        "token".to_string(),
        registry_with_slow(Duration::from_millis(300)), // engine sleeps 300 ms
    )
    .with_execution_timeout(Duration::from_millis(50)); // backstop fires at 50 ms

    executor
        .preload_for_transaction("test_tx_001")
        .await
        .expect("preload should be admitted");

    // Must settle quickly (the backstop fires at 50 ms) — not hang. `None` would
    // mean the task never settled, i.e. the unbounded path the backstop prevents.
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        wait_for_settle(&executor, "test_tx_001"),
    )
    .await
    .expect("execution must settle within the backstop — it must not hang")
    .expect("task should settle");

    // The wall-clock backstop fired: the tx failed with `ExecutionTimeout`.
    let err = outcome.expect_err("a timed-out engine must fail the transaction");
    assert!(
        err.contains("ExecutionTimeout"),
        "expected an ExecutionTimeout failure, got: {}",
        err
    );

    // The tx transitioned to Failed in the pending registry (not left in
    // Executing).
    let entry = pending_registry
        .get_transaction_mut("test_tx_001")
        .expect("tx still in registry");
    assert!(
        matches!(entry.state, TransactionState::Failed { .. }),
        "tx should be in a Failed state, got {:?}",
        entry.state
    );

    // The backpressure slot was freed (the timeout path releases it).
    assert_eq!(executor.in_flight_count().await, 0, "slot must be freed after timeout");
}

// --- Panic isolation: a panicking engine fails the tx, not the task ---

#[tokio::test]
async fn panic_isolated_fails_tx_not_crash() {
    let node_registry = make_test_node_registry();
    let pending_registry = make_test_pending_registry(); // test_tx_001
    let data_provider = engine_data_provider(vec![0, 1, 2], vec![10, 20, 30], "Boom");

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry.clone(),
        make_test_hash_provider(),
        100,
        "token".to_string(),
        registry_with_boom(),
    );

    executor
        .preload_for_transaction("test_tx_001")
        .await
        .expect("preload should be admitted");

    let outcome = wait_for_settle(&executor, "test_tx_001")
        .await
        .expect("task should settle");
    let err = outcome.expect_err("a panicking engine must fail the transaction");
    assert!(
        err.contains("ContractExecutionFailed"),
        "expected a ContractExecutionFailed failure, got: {}",
        err
    );

    // The tx transitioned to Failed, and the task is alive (the slot is freed).
    let entry = pending_registry
        .get_transaction_mut("test_tx_001")
        .expect("tx still in registry");
    assert!(
        matches!(entry.state, TransactionState::Failed { .. }),
        "tx should be in a Failed state, got {:?}",
        entry.state
    );
    assert_eq!(executor.in_flight_count().await, 0, "slot must be freed after a panic");
}

// --- Backpressure under timeout load: slots are freed even on timeout ---

#[tokio::test]
async fn backpressure_under_timeout_load() {
    // Three transactions that each time out (SlowEngine sleeps past the
    // backstop). At `max_in_flight = 1`, a leaked slot would make the second
    // preload hit `AtCapacity`; the timeout path frees the slot, so all three
    // are admitted in turn.
    let node_registry = make_test_node_registry();
    let data_provider = engine_data_provider(vec![0, 1, 2], vec![10, 20, 30], "Slow");
    let pending_registry = Arc::new(PendingTransactionRegistry::new());

    for i in 0..3usize {
        let tx_id = format!("tmo_tx_{}", i);
        let tx = Transaction {
            payload: vec![],
            gas_limit: 0,
            id: tx_id.clone(),
            action: "Transfer".to_string(),
            token_id: vec![0, 1, 2],
            bid: None,
            sequence_number: 1,
            sender: vec![10, 20, 30],
            receiver: vec![40, 50, 60],
            amount: Some(100),
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
        registry_with_slow(Duration::from_millis(120)),
    )
    .with_execution_timeout(Duration::from_millis(40));

    for i in 0..3usize {
        let tx_id = format!("tmo_tx_{}", i);
        // Wait for any in-flight task to settle (via its timeout) first.
        while executor.in_flight_count().await > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let r = executor.preload_for_transaction(&tx_id).await;
        assert!(
            r.is_ok(),
            "preload {} should be admitted (slot freed on timeout), got {:?}",
            i,
            r
        );
    }
}
