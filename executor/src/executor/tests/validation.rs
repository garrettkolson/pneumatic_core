//! Execution-result validation: the free `validate_execution_result` checks
//! result-data presence, result-hash presence, the gas cap, and the
//! transfer-delta post-condition.
use super::helpers::*;
use super::super::*;

fn make_tx() -> Transaction {
    Transaction {
        payload: vec![],
        gas_limit: 0,
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
    
    result_data: vec![],}
}

#[test]
fn validate_execution_result_empty_result_data_fails() {
    let tx = make_tx();
    let result = ExecutionResult {
        transaction_id: "test_tx_001".to_string(),
        result_data: vec![], // empty data should fail
        result_hash: vec![9, 8, 7, 6],
        gas_used: 0,
    };
    let validation = validate_execution_result(&tx, &result);
    assert!(validation.is_err());
    let reason_str = format!("{:?}", validation.unwrap_err());
    assert!(reason_str.contains("ContractExecutionFailed"));
}

#[test]
fn validate_execution_result_empty_hash_fails() {
    let tx = make_tx();
    let result = ExecutionResult {
        transaction_id: "test_tx_001".to_string(),
        result_data: vec![1, 2, 3],
        result_hash: vec![], // empty hash should fail
        gas_used: 0,
    };
    let validation = validate_execution_result(&tx, &result);
    assert!(validation.is_err());
    let reason_str = format!("{:?}", validation.unwrap_err());
    assert!(reason_str.contains("MissingResultHash"));
}

#[test]
fn validate_execution_result_gas_over_cap_fails() {
    let mut tx = make_tx();
    tx.gas_limit = 100;
    let result = ExecutionResult {
        transaction_id: "test_tx_001".to_string(),
        result_data: vec![1, 2, 3],
        result_hash: vec![9, 8, 7, 6],
        gas_used: 101, // exceeds the declared cap
    };
    let validation = validate_execution_result(&tx, &result);
    assert!(validation.is_err());
    let reason_str = format!("{:?}", validation.unwrap_err());
    assert!(reason_str.contains("GasLimitExceeded"));
}

#[test]
fn validate_execution_result_transfer_mismatch_fails() {
    let tx = make_tx();
    // A transfer delta whose amount does not match tx.amount.
    let delta = pneumatic_core::contracts::TransferDelta {
        token_id: tx.token_id.clone(),
        sender: tx.sender.clone(),
        receiver: tx.receiver.clone(),
        amount: 999, // mismatch
        sequence_number: tx.sequence_number,
    };
    let result_data = pneumatic_core::encoding::serialize_to_bytes_rmp(&delta).unwrap();
    let result = ExecutionResult {
        transaction_id: "test_tx_001".to_string(),
        result_data,
        result_hash: vec![9, 8, 7, 6],
        gas_used: 21_000,
    };
    let validation = validate_execution_result(&tx, &result);
    assert!(validation.is_err());
    let reason_str = format!("{:?}", validation.unwrap_err());
    assert!(reason_str.contains("InvalidAmount"));
}

#[test]
fn validate_execution_result_valid_transfer_succeeds() {
    let tx = make_tx();
    // A transfer delta that matches the transaction.
    let delta = pneumatic_core::contracts::TransferDelta {
        token_id: tx.token_id.clone(),
        sender: tx.sender.clone(),
        receiver: tx.receiver.clone(),
        amount: 100, // matches tx.amount
        sequence_number: tx.sequence_number,
    };
    let result_data = pneumatic_core::encoding::serialize_to_bytes_rmp(&delta).unwrap();
    let result = ExecutionResult {
        transaction_id: "test_tx_001".to_string(),
        result_data,
        result_hash: vec![9, 8, 7, 6],
        gas_used: 21_000,
    };
    let validation = validate_execution_result(&tx, &result);
    assert!(validation.is_ok());
}
