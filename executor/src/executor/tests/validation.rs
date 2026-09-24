//! Execution-result validation: an empty result hash fails, a non-empty
//! one succeeds.
use super::helpers::*;
use super::super::*;

// --- ExecutionResult validation ---

#[test]
fn validate_execution_result_empty_hash_fails() {
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

    let tx = Transaction {
        id: "test_tx_001".into(),
        action: "Transfer".into(),
        token_id: vec![0, 1, 2],
        bid: None,
        sequence_number: 1,
        sender: vec![10, 20, 30],
        receiver: vec![40, 50, 60],
        amount: Some(100),
        timestamp: 1000,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let result = ExecutionResult {
        transaction_id: "test_tx_001".to_string(),
        result_data: vec![1, 2, 3],
        result_hash: vec![], // empty hash should fail validation
    };
    let validation = executor.validate_execution_result(&tx, &result);
    assert!(validation.is_err());
    let reasons = validation.unwrap_err();
    // Verify ContractNotFound is among the reasons by matching display
    let reason_str = format!("{:?}", reasons);
    assert!(reason_str.contains("ContractNotFound"));
}

#[test]
fn validate_execution_result_nonempty_hash_succeeds() {
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

    let tx = Transaction {
        id: "test_tx_001".into(),
        action: "Transfer".into(),
        token_id: vec![0, 1, 2],
        bid: None,
        sequence_number: 1,
        sender: vec![10, 20, 30],
        receiver: vec![40, 50, 60],
        amount: Some(100),
        timestamp: 1000,
        result_hash: vec![],
        sender_signature: vec![],
    };
    let result = ExecutionResult {
        transaction_id: "test_tx_001".to_string(),
        result_data: vec![1, 2, 3],
        result_hash: vec![9, 8, 7, 6], // non-empty hash
    };
    let validation = executor.validate_execution_result(&tx, &result);
    assert!(validation.is_ok());
}
