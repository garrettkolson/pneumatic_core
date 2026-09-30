//! ADR-017, Phase 8: the committer's `ReplaceAssetDelta` apply path.
//!
//! The executor emits a canonical `ReplaceAssetDelta` in `result_data` and the
//! finalizer signs its hash. The delta is a **pure function** of the
//! transaction (`token_id`, `UpgradeParams`) — so `apply_upgrade_delta` re-derives
//! it, verifies it against the signed `result_hash`, and — only when the
//! committed block's `epoch_number` satisfies the 1-epoch timelock (QD3) — swaps
//! the target contract's bytecode + owner set. The committer does **not** re-check
//! the quorum (the executor and sentinel already did); it only binds the apply to
//! the signed output and the timelock. The apply is a no-op for a missing token,
//! an immutable target (`threshold == 0`), an unsatisfied timelock, and is
//! idempotent under replay.

use super::helpers::*;
use pneumatic_core::contracts::{ReplaceAssetDelta, UpgradeParams};
use pneumatic_core::crypto::HashProvider;
use pneumatic_core::tokens::{SmartContract, Token};
use std::sync::Arc;

/// QD4: the upgrade apply reads/writes the token under `partition_id ==
/// environment_id` (the committer test environment id is `"test"`).
const PARTITION: &str = "test";

/// A contract token (metadata `contract_engine == "Spec"`, a `SmartContract`
/// asset with the given owner registry + threshold).
fn contract_token(token_id: Vec<u8>, owners: Vec<Vec<u8>>, threshold: u32) -> Token {
    let mut token = Token::new();
    token.id = token_id;
    token.environment_id = "test".to_string();
    token
        .metadata
        .insert("token_type".to_string(), "contract".to_string());
    token
        .metadata
        .insert("contract_engine".to_string(), "Spec".to_string());
    let sc = SmartContract {
        name: "c".to_string(),
        bytecode: vec![1, 2, 3],
        version: "1".to_string(),
        storage: Default::default(),
        owners,
        threshold,
    };
    token.set_asset(&sc).unwrap();
    token
}

/// Build an `UpgradeContract` transaction whose `result_hash` is the canonical
/// hash of the `ReplaceAssetDelta` the committer will re-derive (so the integrity
/// check in `apply_upgrade_delta` passes). The committer ignores
/// `owner_signatures` (quorum is the executor's / sentinel's job).
fn make_upgrade_tx(
    token_id: Vec<u8>,
    new_bytecode: Vec<u8>,
    new_owners: Vec<Vec<u8>>,
    new_threshold: u32,
    proposal_epoch: u64,
) -> Transaction {
    let hash = BasicHashProvider::new();
    let params = UpgradeParams {
        new_bytecode: new_bytecode.clone(),
        new_owners: new_owners.clone(),
        new_threshold,
        proposal_epoch,
        owner_signatures: vec![],
    };
    let delta = ReplaceAssetDelta {
        token_id: token_id.clone(),
        new_bytecode,
        new_owners,
        new_threshold,
        proposal_epoch,
    };
    let result_bytes = serialize_to_bytes_rmp(&delta).unwrap();
    let result_hash = hash.hash(&result_bytes);
    Transaction {
        payload: serialize_to_bytes_rmp(&params).unwrap(),
        gas_limit: 0,
        id: "upgrade_tx".to_string(),
        action: "UpgradeContract".to_string(),
        token_id,
        bid: None,
        sequence_number: 0,
        sender: vec![0x42; 32],
        receiver: vec![],
        amount: None,
        timestamp: 0,
        result_hash,
        sender_signature: vec![],
        result_data: vec![],
    }
}

/// When the committed block's epoch satisfies the 1-epoch timelock, the swap is
/// applied: the contract's bytecode + owner set are replaced, and name/version/
/// storage are preserved.
#[test]
fn upgrade_delta_applies_when_timelock_satisfied() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![1, 2, 3];
    dp.insert_token(
        token_id.clone(),
        PARTITION.to_string(),
        contract_token(token_id.clone(), vec![vec![0xAA]], 1),
    );

    // proposal_epoch = 5 → applies at epoch 6 (5 + 1).
    let tx = make_upgrade_tx(
        token_id.clone(),
        vec![9, 8, 7],
        vec![vec![0xBB]],
        1,
        5,
    );
    committer
        .apply_upgrade_delta(&tx, 6)
        .expect("upgrade should apply");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    assert_eq!(sc.bytecode, vec![9, 8, 7]);
    assert_eq!(sc.owners, vec![vec![0xBB]]);
    assert_eq!(sc.threshold, 1);
    // name / version / storage preserved.
    assert_eq!(sc.name, "c");
    assert_eq!(sc.version, "1");
}

/// A commit BEFORE the timelock window (epoch < proposal_epoch + 1) is a no-op:
/// the proposal is admitted and signed, but the asset is not yet swapped.
#[test]
fn upgrade_delta_noop_before_timelock() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![4, 5, 6];
    dp.insert_token(
        token_id.clone(),
        PARTITION.to_string(),
        contract_token(token_id.clone(), vec![vec![0xAA]], 1),
    );

    // proposal_epoch = 10 → NOT satisfied at epoch 5 (needs 11).
    let tx = make_upgrade_tx(
        token_id.clone(),
        vec![9, 8, 7],
        vec![vec![0xBB]],
        1,
        10,
    );
    committer
        .apply_upgrade_delta(&tx, 5)
        .expect("early commit is a no-op");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    // Unchanged.
    assert_eq!(sc.bytecode, vec![1, 2, 3]);
    assert_eq!(sc.owners, vec![vec![0xAA]]);
    assert_eq!(sc.threshold, 1);
}

/// The owner set is rotated to the proposed owners (safe rotation: the new
/// owners take effect at apply time, not at proposal time).
#[test]
fn upgrade_delta_owner_rotation() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![7, 8, 9];
    dp.insert_token(
        token_id.clone(),
        PARTITION.to_string(),
        contract_token(token_id.clone(), vec![vec![0x01], vec![0x02]], 2),
    );

    // Rotate to a completely new owner set + threshold.
    let tx = make_upgrade_tx(
        token_id.clone(),
        vec![0xEE],
        vec![vec![0x03], vec![0x04]],
        2,
        1,
    );
    committer
        .apply_upgrade_delta(&tx, 2)
        .expect("rotation should apply");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    assert_eq!(sc.owners, vec![vec![0x03], vec![0x04]]);
    assert_eq!(sc.threshold, 2);
    assert_eq!(sc.bytecode, vec![0xEE]);
}

/// A missing token is a no-op (nothing to apply).
#[test]
fn upgrade_delta_noop_for_missing_token() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    // No token inserted.
    let tx = make_upgrade_tx(vec![99], vec![1], vec![], 1, 1);
    committer
        .apply_upgrade_delta(&tx, 2)
        .expect("missing token is a no-op");
}

/// An immutable target (`threshold == 0`) is a no-op, even when the timelock is
/// satisfied: the contract's bytecode + owner set are untouched.
#[test]
fn upgrade_delta_noop_for_immutable_threshold_zero() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![10, 11, 12];
    dp.insert_token(
        token_id.clone(),
        PARTITION.to_string(),
        contract_token(token_id.clone(), vec![vec![0xAA]], 0),
    );

    let tx = make_upgrade_tx(
        token_id.clone(),
        vec![9, 8, 7],
        vec![vec![0xBB]],
        1,
        1,
    );
    committer
        .apply_upgrade_delta(&tx, 2)
        .expect("immutable target is a no-op");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    assert_eq!(sc.bytecode, vec![1, 2, 3]);
    assert_eq!(sc.owners, vec![vec![0xAA]]);
    assert_eq!(sc.threshold, 0);
}

/// A tampered `result_hash` fails closed (integrity check) — the re-derived
/// delta no longer matches the signed hash.
#[test]
fn upgrade_delta_rejects_tampered_result_hash() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![13, 14, 15];
    dp.insert_token(
        token_id.clone(),
        PARTITION.to_string(),
        contract_token(token_id.clone(), vec![vec![0xAA]], 1),
    );

    let mut tx = make_upgrade_tx(token_id.clone(), vec![9], vec![], 1, 1);
    tx.result_hash = vec![0xFF; 32];
    let err = committer
        .apply_upgrade_delta(&tx, 2)
        .expect_err("tampered hash must be rejected");
    assert!(matches!(
        err,
        CommitterError::TransactionPayloadMismatch(_)
    ));
}

/// Re-applying the same delta (a replayed commit at a satisfying epoch) is a
/// no-op, not an error.
#[test]
fn upgrade_delta_is_idempotent_on_replay() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());
    let token_id = vec![16, 17, 18];
    dp.insert_token(
        token_id.clone(),
        PARTITION.to_string(),
        contract_token(token_id.clone(), vec![vec![0xAA]], 1),
    );

    let tx = make_upgrade_tx(token_id.clone(), vec![9, 8, 7], vec![vec![0xBB]], 1, 1);
    committer
        .apply_upgrade_delta(&tx, 2)
        .expect("first apply");
    committer
        .apply_upgrade_delta(&tx, 2)
        .expect("replay is a no-op");

    let token = dp.get_token(&token_id, PARTITION).expect("token should exist");
    let sc: SmartContract = token.get_asset().expect("contract asset");
    assert_eq!(sc.bytecode, vec![9, 8, 7]);
    assert_eq!(sc.owners, vec![vec![0xBB]]);
}
