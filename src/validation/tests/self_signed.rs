//! Self-signed spec tests: owner-is-sender validation (hex + real Ed25519
//! keys), risk party counting, and the self-verified block flow.
use super::helpers::*;
use super::super::*;

// --- SelfSignedBlockValidatorSpec ---

#[test]
fn self_signed_validates_sender_is_owner() {
    let spec = SelfSignedBlockValidatorSpec::new();
    let token = make_token_with_owner(&[1, 2, 3]);
    let tx = make_tx(&[1, 2, 3], &[], None, 1);
    let env = make_env_with_defaults();
    let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
    assert!(result.is_ok());
    assert!(result.unwrap().is_valid);
}

#[test]
fn self_signed_rejects_sender_not_owner() {
    let spec = SelfSignedBlockValidatorSpec::new();
    let token = make_token_with_owner(&[1, 2, 3]);
    let tx = make_tx(&[9, 9, 9], &[], None, 1);
    let env = make_env_with_defaults();
    let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
    assert!(result.is_err());
    match result.unwrap_err() {
        PneumaticError::Validation(ref reasons) => {
            let reason_str = format!("{:?}", reasons);
            assert!(reason_str.contains("NotTokenOwner"));
        }
        _ => panic!("expected Validation error"),
    }
}

#[test]
fn self_signed_rejects_missing_owner_metadata() {
    let spec = SelfSignedBlockValidatorSpec::new();
    let token = Token::new(); // no owner metadata
    let tx = make_tx(&[1, 2, 3], &[], None, 1);
    let env = make_env_with_defaults();
    let result = TransactionValidationSpec::validate(&spec, &tx, &token, &env);
    assert!(result.is_err());
}

// --- AUDIT 5.9 discriminators: the owner is a hex-encoded key ---

#[test]
fn self_signed_accepts_real_ed25519_key_owner() {
    // Audit acceptance: an owner can execute a self-signed operation. A real
    // Ed25519 pubkey (32 bytes) is the owner, hex-encoded in metadata; the
    // signer is that same key. This only works once the owner is a hex string
    // rather than a raw-key String, since a 32-byte key is not valid UTF-8.
    let identity = NodeIdentity::generate_in_memory();
    let owner_pk = identity.ed25519.public_key().expect("owner public key");

    let mut token = Token::new();
    token.set_metadata("owner".to_string(), hex::encode(&owner_pk));
    let tx = make_tx(&owner_pk, &[], Some(100), 1);
    let env = make_env_with_defaults();
    let result =
        TransactionValidationSpec::validate(&SelfSignedBlockValidatorSpec::new(), &tx, &token, &env);
    assert!(result.is_ok(), "owner-key owner must be accepted, got {result:?}");
}

#[test]
fn self_signed_accepts_non_utf8_key_bytes() {
    // Discriminator for the String-vs-bytes bug: a non-UTF-8 byte payload
    // (0x80 has no valid UTF-8 meaning) can be stored only as hex. The old
    // String::from_utf8(owner) round-trip would panic on it; hex::decode does
    // not, so the owner (these exact bytes) validates as sender.
    let owner: Vec<u8> = vec![0x80u8; 32];
    let mut token = Token::new();
    token.set_metadata("owner".to_string(), hex::encode(&owner));
    let tx = make_tx(&owner, &[], Some(100), 1);
    let env = make_env_with_defaults();
    let result =
        TransactionValidationSpec::validate(&SelfSignedBlockValidatorSpec::new(), &tx, &token, &env);
    assert!(result.is_ok(), "non-UTF-8 key owner must validate, got {result:?}");
}

#[test]
fn self_signed_rejects_unparseable_owner() {
    // Fail closed: an owner that is not valid hex (e.g. a stray non-hex byte
    // from a corrupt write) yields NotTokenOwner, never a panic or accept.
    let mut token = Token::new();
    token.set_metadata("owner".to_string(), "not-hex-value!".to_string());
    let tx = make_tx(&[1, 2, 3], &[], Some(100), 1);
    let env = make_env_with_defaults();
    let result =
        TransactionValidationSpec::validate(&SelfSignedBlockValidatorSpec::new(), &tx, &token, &env);
    assert!(matches!(
        result,
        Err(PneumaticError::Validation(ref reasons))
            if reasons.iter().any(|r| matches!(r, ValidationFailureReason::NotTokenOwner))
    ), "unparseable owner must fail closed as NotTokenOwner, got {result:?}");
}

#[test]
fn self_signed_risk_with_receiver_counts_two_parties() {
    let spec = SelfSignedBlockValidatorSpec::new();
    let tx = make_tx(&[1], &[2], Some(100), 1);
    let risk = spec.calculate_risk(&tx);
    assert_eq!(risk.affected_parties, 2);
}

#[test]
fn self_signed_risk_no_receiver_counts_one_party() {
    let spec = SelfSignedBlockValidatorSpec::new();
    let tx = make_tx(&[1], &[], Some(100), 1);
    let risk = spec.calculate_risk(&tx);
    assert_eq!(risk.affected_parties, 1);
}

// --- T08: Self-Validated Token Flow (minimal integration) ---

#[test]
fn self_signed_token_flow_end_to_end() {
    // Create token with owner metadata. Owner is stored as hex (AUDIT 5.9)
    // so hex::decode(b"alice") round-trips to the sender's real key bytes.
    let mut token = Token::new();
    token.set_metadata("owner".to_string(), hex::encode(b"alice"));

    // Create transaction with matching sender
    let tx = Transaction {
        id: "tx_self_signed".into(),
        action: "Transfer".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: b"alice".to_vec(),
        receiver: vec![],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    };

    // Validate with SelfSigned spec
    let spec = SelfSignedBlockValidatorSpec::new();
    let env = make_env_with_defaults();
    let validation_result = TransactionValidationSpec::validate(&spec, &tx, &token, &env).unwrap();
    assert!(validation_result.is_valid);

    // Transition to Validated state in PendingTransaction
    let pt_id = tx.id.clone();
    let mut pt = PendingTransaction::new(pt_id.clone(), TransactionState::Pending);
    pt.transition_to_validated(tx, validation_result);

    // Confirm registry holds validated state
    let registry = PendingTransactionRegistry::new();
    registry.add_transaction(pt_id.clone(), pt).unwrap();
    let result = registry.get_validation_result(&pt_id).unwrap();
    assert!(result.is_valid);
}

// --- SelfSignedBlockValidatorSpec (BlockValidatorSpec trait) ---

#[test]
fn self_signed_block_validates_chain_and_self_verified() {
    let spec = SelfSignedBlockValidatorSpec::new();
    let mut token = make_self_signed_token(&[1, 2, 3]);
    let signed_tx = make_signed_tx_with_fields(vec![], HashMap::new(), TransactionSignature {
        transaction_id: vec![],
        env_id: vec![],
        transaction_hash: vec![],
        signature: vec![0u8; 64],
        current_stake: 10,
    });
    let block = make_valid_block(signed_tx, &mut token.blockchain);
    let env = make_env_with_defaults();
    let result = BlockValidatorSpec::validate(&spec, &block, &token, &env);
    assert!(result.is_ok());
    assert!(matches!(result.unwrap(), BlockValidationResult::Valid));
}

#[test]
fn self_signed_block_rejects_non_self_verified_token() {
    let spec = SelfSignedBlockValidatorSpec::new();
    let mut token = Token::new();
    token.set_metadata("owner".to_string(), hex::encode(vec![1, 2, 3]));
    token.is_self_verified = false;
    token.block_validation_spec_name = String::from("SelfSigned");
    let signed_tx = make_signed_tx_with_fields(vec![], HashMap::new(), TransactionSignature {
        transaction_id: vec![],
        env_id: vec![],
        transaction_hash: vec![],
        signature: vec![0u8; 64],
        current_stake: 10,
    });
    let block = make_valid_block(signed_tx, &mut token.blockchain);
    let env = make_env_with_defaults();
    let result = BlockValidatorSpec::validate(&spec, &block, &token, &env);
    assert!(result.is_err());
    match result.unwrap_err() {
        PneumaticError::Validation(ref reasons) => {
            let reason_str = format!("{:?}", reasons);
            assert!(reason_str.contains("NotSelfVerified"));
        }
        _ => panic!("expected Validation error"),
    }
}
