//! Executor lifecycle tests: creation, capacity + backpressure admission,
//! task cleanup, rejection of unknown/terminal transactions, and the full
//! backpressure cycle.
use super::helpers::*;
use super::super::*;

#[tokio::test]
async fn test_executor_creation() {
    let node_registry = make_test_node_registry();
    let data_provider = make_test_data_provider();
    let pending_registry = make_test_pending_registry();
    let hash_provider = make_test_hash_provider();

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry,
        hash_provider,
        10,
    );

    assert!(!executor.is_at_capacity().await);
    assert_eq!(executor.in_flight_count().await, 0);
}

#[tokio::test]
async fn test_executor_at_capacity() {
    let node_registry = make_test_node_registry();
    let data_provider = make_test_data_provider();
    let pending_registry = make_test_pending_registry();
    let hash_provider = make_test_hash_provider();

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry,
        hash_provider,
        1,
    );

    assert!(!executor.is_at_capacity().await);

    // Manually simulate a task being in-flight
    let task_results = Arc::new(DashMap::new());
    executor.preload_tasks.lock().await.insert("test_tx_001".to_string(), task_results);

    assert!(executor.is_at_capacity().await);
    assert_eq!(executor.in_flight_count().await, 1);
}

#[tokio::test]
async fn test_executor_backpressure_rejects() {
    let node_registry = make_test_node_registry();
    let data_provider = make_test_data_provider();
    let pending_registry = Arc::new(PendingTransactionRegistry::new());
    let hash_provider = make_test_hash_provider();

    // Add a valid preloaded transaction so the capacity check is reached
    let tx = Transaction {
        id: "capacity_tx".to_string(),
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
    };
    let pending = PendingTransaction::new("capacity_tx".to_string(), TransactionState::Preloaded { transaction: tx });
    let _ = pending_registry.add_transaction("capacity_tx".to_string(), pending);

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry,
        hash_provider,
        0, // max_in_flight = 0, always at capacity
    );

    let result = executor.preload_for_transaction("capacity_tx").await;
    assert!(result.is_err());

    if let Err(ExecutorError::AtCapacity { max_in_flight, .. }) = result {
        assert_eq!(max_in_flight, 0);
    } else {
        panic!("Expected AtCapacity error, got {:?}", result);
    }
}

#[tokio::test]
async fn test_executor_rejects_nonexistent_transaction() {
    let node_registry = make_test_node_registry();
    let data_provider = make_test_data_provider();
    let pending_registry = make_test_pending_registry();
    let hash_provider = make_test_hash_provider();

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry,
        hash_provider,
        100,
    );

    let result = executor.preload_for_transaction("nonexistent_tx").await;
    assert!(result.is_err());
    if let Err(ExecutorError::Registry(msg)) = result {
        assert!(msg.contains("nonexistent_tx"));
    } else {
        panic!("Expected Registry error, got {:?}", result);
    }
}

#[tokio::test]
async fn test_executor_rejects_transaction_in_terminal_state() {
    let node_registry = make_test_node_registry();
    let data_provider = make_test_data_provider();
    let pending_registry = Arc::new(PendingTransactionRegistry::new());
    let hash_provider = make_test_hash_provider();

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry.clone(),
        hash_provider,
        100,
    );

    // Add a transaction in Failed state
    let tx = Transaction {
        id: "failed_tx".to_string(),
        action: "Transfer".to_string(),
        token_id: vec![],
        bid: None,
        sequence_number: 1,
        sender: vec![],
        receiver: vec![],
        amount: Some(0),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let pending = PendingTransaction::new("failed_tx".to_string(), TransactionState::Failed {
        transaction: tx,
        reasons: vec![],
    });
    let _ = pending_registry.add_transaction("failed_tx".to_string(), pending);

    let result = executor.preload_for_transaction("failed_tx").await;
    assert!(result.is_err());
    if let Err(ExecutorError::Registry(msg)) = result {
        assert!(msg.contains("failed_tx"));
    } else {
        panic!("Expected Registry error for terminal state, got {:?}", result);
    }
}

#[tokio::test]
async fn test_executor_cleanup_removes_task() {
    let node_registry = make_test_node_registry();
    let data_provider = make_test_data_provider();
    let pending_registry = make_test_pending_registry();
    let hash_provider = make_test_hash_provider();

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry,
        hash_provider,
        10,
    );

    // Manually add a task
    let task_results = Arc::new(DashMap::new());
    executor.preload_tasks.lock().await.insert("cleanup_test".to_string(), task_results);

    assert_eq!(executor.in_flight_count().await, 1);

    executor.preload_cleanup("cleanup_test").await;

    assert_eq!(executor.in_flight_count().await, 0);
}

// --- Full backpressure cycle ---

#[tokio::test]
async fn full_backpressure_cycle() {
    let node_registry = make_test_node_registry();
    let data_provider = make_test_data_provider();
    let pending_registry = Arc::new(PendingTransactionRegistry::new());
    let hash_provider = make_test_hash_provider();

    // Add two transactions to the registry
    for tx_id in ["bp_tx_a", "bp_tx_b"] {
        let tx = Transaction {
            id: tx_id.to_string(),
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
        };
        let pending = PendingTransaction::new(
            tx_id.to_string(),
            TransactionState::Preloaded { transaction: tx },
        );
        let _ = pending_registry.add_transaction(tx_id.to_string(), pending);
    }

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
        node_registry,
        data_provider,
        pending_registry,
        hash_provider,
        1, // capacity of 1
    );

    // First preload should succeed (slot available)
    let result_a = executor.preload_for_transaction("bp_tx_a").await;
    assert!(result_a.is_ok());
    assert_eq!(executor.in_flight_count().await, 1);
    assert!(executor.is_at_capacity().await);

    // Second preload should fail due to backpressure
    let result_b = executor.preload_for_transaction("bp_tx_b").await;
    assert!(result_b.is_err());
    if let Err(ExecutorError::AtCapacity { max_in_flight, .. }) = result_b {
        assert_eq!(max_in_flight, 1);
    } else {
        panic!("Expected AtCapacity error, got {:?}", result_b);
    }

    // Cleanup the first task frees a slot
    executor.preload_cleanup("bp_tx_a").await;
    assert_eq!(executor.in_flight_count().await, 0);
    assert!(!executor.is_at_capacity().await);

    // Now the second preload should succeed
    let result_b = executor.preload_for_transaction("bp_tx_b").await;
    assert!(result_b.is_ok());
    assert_eq!(executor.in_flight_count().await, 1);
}
