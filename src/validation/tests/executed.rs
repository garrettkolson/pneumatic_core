//! Executed spec tests: sender/nonce/amount floors, the nonce validation
//! ladder, and the executed-block requirements (result hash, executor
//! sigs, finalizer signature, risk ceiling).
use super::helpers::*;
use super::super::*;

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
