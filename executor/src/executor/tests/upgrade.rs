//! ADR-017, Phase 8: the executor's `UpgradeContract` protocol op end-to-end.
//!
//! An `UpgradeContract` transaction is dispatched to `execute_upgrade` (the
//! contract path — it has a target token, but no engine execution). The
//! executor re-validates the M-of-N owner quorum deterministically (QD2), then
//! emits a canonical `ReplaceAssetDelta` in `result_data` and votes to the
//! finalizer over its hash. These tests drive a full `run_execution` and assert
//! the delta, the result hash, the gas model, and the quorum / immutable
//! failure paths.

use super::helpers::*;
use super::super::*;

use pneumatic_core::contracts::{upgrade_digest, upgrade_gas, UpgradeParams};
use pneumatic_core::crypto::{AsymCryptoProvider, Ed25519Provider};
use pneumatic_core::data::StubDataProvider;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::tokens::{SmartContract, Token};
use pneumatic_core::transactions::{PendingTransaction, Transaction, TransactionState};
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

/// A data provider holding a contract token (with owner registry + threshold)
/// and a sender user, both under the `"token"` partition — the shape
/// `run_execution` expects for a full successful dispatch.
fn upgrade_data_provider(
    token_id: Vec<u8>,
    sender: Vec<u8>,
    owners: Vec<Vec<u8>>,
    threshold: u32,
) -> Arc<dyn DataProvider> {
    let contract = SmartContract {
        name: "c".to_string(),
        bytecode: vec![1, 2, 3],
        version: "1".to_string(),
        storage: Default::default(),
        owners,
        threshold,
    };
    let token = Token::from_asset(&contract).unwrap().with_id(token_id.clone());
    let user = User::new(sender.clone());
    Arc::new(
        StubDataProvider::new()
            .with_token(token_id, "token".to_string(), token)
            .with_user(sender, "token".to_string(), user),
    )
}

/// Build an `UpgradeContract` pending-registry entry.
fn upgrade_entry(
    params: UpgradeParams,
    token_id: Vec<u8>,
    sender: Vec<u8>,
    nonce: usize,
) -> (PendingTransaction, UpgradeParams) {
    let tx = Transaction {
        payload: serialize_to_bytes_rmp(&params).unwrap(),
        gas_limit: 0,
        id: "upgrade_tx_001".to_string(),
        action: "UpgradeContract".to_string(),
        token_id,
        bid: None,
        sequence_number: nonce,
        sender,
        receiver: vec![],
        amount: None,
        timestamp: 1000,
        result_hash: vec![],
        sender_signature: vec![],
        result_data: vec![],
    };
    let pending = PendingTransaction::new(
        "upgrade_tx_001".to_string(),
        TransactionState::Preloaded { transaction: tx },
    );
    (pending, params)
}

/// A new Spec program (keeps the delta realistic; the executor path does not
/// scan, but the committer / sentinel would).
fn new_spec_bytecode() -> Vec<u8> {
    serialize_to_bytes_rmp(&pneumatic_core::contracts::InstructionProgram {
        version: 1,
        ops: vec![
            pneumatic_core::contracts::Op::LoadConst(1),
            pneumatic_core::contracts::Op::Emit,
            pneumatic_core::contracts::Op::Halt,
        ],
    })
    .unwrap()
}

// --- Happy path: quorum met -> ReplaceAssetDelta + result hash + gas ---

#[tokio::test]
async fn upgrade_produces_replace_asset_delta_and_result_hash() {
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

    let token_id = vec![0x1, 0x2, 0x3];
    let sender = vec![0x42; 32];

    // Two owners, threshold 2 (M-of-N with M = N = 2); both sign.
    let o1 = Ed25519Provider::generate();
    let o2 = Ed25519Provider::generate();
    let pk1 = o1.public_key().unwrap();
    let pk2 = o2.public_key().unwrap();

    let new_bytecode = new_spec_bytecode();
    let new_owners = vec![pk1.clone(), pk2.clone()];
    let new_threshold = 2u32;
    let proposal_epoch = 9u64;

    let hash = pneumatic_core::crypto::BasicHashProvider::new();
    let digest = upgrade_digest(
        &token_id,
        &new_bytecode,
        &new_owners,
        new_threshold,
        proposal_epoch,
        &hash,
    );

    let params = UpgradeParams {
        new_bytecode: new_bytecode.clone(),
        new_owners: new_owners.clone(),
        new_threshold,
        proposal_epoch,
        owner_signatures: vec![o1.sign_data(&digest).unwrap(), o2.sign_data(&digest).unwrap()],
    };
    let (pending, params) = upgrade_entry(params, token_id.clone(), sender, 7);

    let pending_registry = Arc::new(pneumatic_core::registry::PendingTransactionRegistry::new());
    pending_registry
        .add_transaction("upgrade_tx_001".to_string(), pending)
        .unwrap();

    let data_provider =
        upgrade_data_provider(token_id.clone(), vec![0x42; 32], vec![pk1.clone(), pk2.clone()], 2);
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
        .preload_for_transaction("upgrade_tx_001")
        .await
        .expect("preload should be admitted");

    let outcome = wait_for_settle(&executor, "upgrade_tx_001")
        .await
        .expect("task should settle");
    let result = outcome.expect("upgrade should succeed");

    // Re-derive the expected delta (same formula as the executor) and assert
    // the result bytes, hash, and gas model match.
    let expected_delta = pneumatic_core::contracts::ReplaceAssetDelta {
        token_id: token_id.clone(),
        new_bytecode,
        new_owners,
        new_threshold,
        proposal_epoch,
    };
    let expected_bytes = serialize_to_bytes_rmp(&expected_delta).unwrap();
    let expected_hash = hash_provider.hash(&expected_bytes);

    assert_eq!(result.result_data, expected_bytes);
    assert_eq!(result.result_hash, expected_hash);
    // Gas model: base + per-byte.
    assert_eq!(result.gas_used, upgrade_gas(expected_delta.new_bytecode.len()));
}

// --- Failure path: M-1 signatures -> quorum fails -> Failed ---

#[tokio::test]
async fn upgrade_quorum_shortfall_reverts() {
    let identity = Arc::new(NodeIdentity::generate_in_memory());
    let node_registry = make_test_node_registry();
    node_registry.register_peer(
        vec![0xAB; 32],
        [5u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(RecordingConnection {
            recorder: Arc::new(std::sync::Mutex::new(Vec::new())),
        }),
    );

    let token_id = vec![0x1, 0x2, 0x3];
    let sender = vec![0x42; 32];

    // Two owners, threshold 2 — only o1 signs (M-1).
    let o1 = Ed25519Provider::generate();
    let o2 = Ed25519Provider::generate();
    let pk1 = o1.public_key().unwrap();
    let pk2 = o2.public_key().unwrap();

    let new_bytecode = new_spec_bytecode();
    let new_owners = vec![pk1.clone(), pk2.clone()];
    let new_threshold = 2u32;
    let proposal_epoch = 9u64;

    let hash = pneumatic_core::crypto::BasicHashProvider::new();
    let digest = upgrade_digest(
        &token_id,
        &new_bytecode,
        &new_owners,
        new_threshold,
        proposal_epoch,
        &hash,
    );

    let params = UpgradeParams {
        new_bytecode,
        new_owners,
        new_threshold,
        proposal_epoch,
        // Only one of two required signatures.
        owner_signatures: vec![o1.sign_data(&digest).unwrap()],
    };
    let (pending, _params) = upgrade_entry(params, token_id.clone(), sender.clone(), 7);

    let pending_registry = Arc::new(pneumatic_core::registry::PendingTransactionRegistry::new());
    pending_registry
        .add_transaction("upgrade_tx_001".to_string(), pending)
        .unwrap();

    let data_provider =
        upgrade_data_provider(token_id, sender, vec![pk1, pk2], 2);

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        identity.clone(),
        node_registry,
        data_provider,
        pending_registry,
        make_test_hash_provider(),
        100,
        "token".to_string(),
        make_test_engine_registry(),
    );

    executor
        .preload_for_transaction("upgrade_tx_001")
        .await
        .expect("preload should be admitted");

    let outcome = wait_for_settle(&executor, "upgrade_tx_001")
        .await
        .expect("task should settle");
    assert!(
        outcome.is_err(),
        "M-1 signatures must fail the upgrade"
    );
}

// --- Failure path: threshold 0 (immutable) -> Failed ---

#[tokio::test]
async fn upgrade_immutable_threshold_zero_reverts() {
    let identity = Arc::new(NodeIdentity::generate_in_memory());
    let node_registry = make_test_node_registry();
    node_registry.register_peer(
        vec![0xAB; 32],
        [5u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(RecordingConnection {
            recorder: Arc::new(std::sync::Mutex::new(Vec::new())),
        }),
    );

    let token_id = vec![0x1, 0x2, 0x3];
    let sender = vec![0x42; 32];

    // A single owner (threshold 1) — but the contract's CURRENT threshold is 0.
    let o1 = Ed25519Provider::generate();
    let pk1 = o1.public_key().unwrap();

    let new_bytecode = new_spec_bytecode();
    let new_owners = vec![pk1.clone()];
    let new_threshold = 1u32;
    let proposal_epoch = 9u64;

    let hash = pneumatic_core::crypto::BasicHashProvider::new();
    let digest = upgrade_digest(
        &token_id,
        &new_bytecode,
        &new_owners,
        new_threshold,
        proposal_epoch,
        &hash,
    );

    let params = UpgradeParams {
        new_bytecode,
        new_owners,
        new_threshold,
        proposal_epoch,
        owner_signatures: vec![o1.sign_data(&digest).unwrap()],
    };
    let (pending, _params) = upgrade_entry(params, token_id.clone(), sender.clone(), 7);

    let pending_registry = Arc::new(pneumatic_core::registry::PendingTransactionRegistry::new());
    pending_registry
        .add_transaction("upgrade_tx_001".to_string(), pending)
        .unwrap();

    // The contract's CURRENT threshold is 0 -> immutable -> no upgrade.
    let data_provider = upgrade_data_provider(token_id, sender, vec![pk1], 0);

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        identity.clone(),
        node_registry,
        data_provider,
        pending_registry,
        make_test_hash_provider(),
        100,
        "token".to_string(),
        make_test_engine_registry(),
    );

    executor
        .preload_for_transaction("upgrade_tx_001")
        .await
        .expect("preload should be admitted");

    let outcome = wait_for_settle(&executor, "upgrade_tx_001")
        .await
        .expect("task should settle");
    assert!(
        outcome.is_err(),
        "an immutable (threshold 0) contract must not be upgradable"
    );
}
