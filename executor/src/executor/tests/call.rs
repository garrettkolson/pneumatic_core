//! Phase 9 (ADR-016): Model X cross-contract calls wired through the executor.
//!
//! Covers the executor-side pieces the plan's exit criteria require:
//! - `ExecutorTargetProvider` resolves a target's pinned state through the data
//!   service (token fetch + ref validated against B's chain + contract asset +
//!   sender user), and **fails closed** on every resolution miss (unknown token,
//!   ref out of range, ref hash mismatch, no contract asset, missing user);
//! - the full `execute_contract` dispatch carries a live [`CallContext`], so a
//!   Spec caller's `Op::Call` resolves its target through the provider and folds
//!   the callee result into the caller output (cross-shard determinism rides on
//!   the pinned ref — every honest executor resolves the same state).

use super::helpers::*;
use super::super::*;

use std::collections::HashMap;

use pneumatic_core::blocks::{Block, BlockFactory, FinalityStatus};
use pneumatic_core::contracts::{InstructionProgram, Op, SnapshotRef, TargetStateProvider};
use pneumatic_core::data::{DataProvider, StubDataProvider};
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::tokens::{SmartContract, Token};
use pneumatic_core::transactions::{SignedTransaction, Transaction};
use pneumatic_core::user::User;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A deterministic block with a computed `current_hash` (mirrors the core
/// `Block::test_block`, which is `pub(crate)` and unreachable from this crate).
fn det_block(prev_hash: Vec<u8>) -> Block {
    let mut block = Block {
        signed_trans: SignedTransaction::test_transaction(),
        token_metadata: HashMap::new(),
        previous_hash: prev_hash,
        current_hash: vec![],
        timestamp: 1_700_000_000,
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash = BlockFactory::create_hash(&block).expect("block hashes");
    block
}

fn spec_bytecode(program: &InstructionProgram) -> Vec<u8> {
    serialize_to_bytes_rmp(program).expect("serialize program")
}

/// A target token carrying a Spec contract asset and a 2-block chain; returns
/// the token and a ref anchored at block 1.
fn chain_token(id: Vec<u8>, program: &InstructionProgram) -> (Token, SnapshotRef) {
    let contract = SmartContract {
        name: "callee".to_string(),
        bytecode: spec_bytecode(program),
        version: "1".to_string(),
        storage: Default::default(),
        owners: vec![],
        threshold: 0,
    };
    let mut token = Token::from_asset(&contract).expect("token from asset");
    token.id = id;
    token
        .metadata
        .insert("token_type".to_string(), "contract".to_string());
    token
        .metadata
        .insert("contract_engine".to_string(), "Spec".to_string());
    let b0 = det_block(vec![]);
    let b1 = det_block(b0.current_hash.clone());
    let h1 = b1.current_hash.clone();
    token.blockchain.add_block(b0);
    token.blockchain.add_block(b1);
    (token, SnapshotRef { height: 1, block_hash: h1 })
}

/// Wrap a stub data service in the executor's provider (partition `"token"`).
fn provider(dp: StubDataProvider) -> super::super::ExecutorTargetProvider {
    super::super::ExecutorTargetProvider {
        data_provider: Arc::new(dp),
        partition_id: "token".to_string(),
    }
}

fn callee_program() -> InstructionProgram {
    InstructionProgram {
        version: 1,
        ops: vec![Op::LoadConst(42), Op::Emit],
    }
}

// ---------------------------------------------------------------------------
// ExecutorTargetProvider — resolve + fail-closed
// ---------------------------------------------------------------------------

#[test]
fn provider_resolves_pinned_target() {
    let (token, r) = chain_token(vec![0xBB], &callee_program());
    let token_id = token.id.clone();
    let dp = StubDataProvider::new()
        .with_token(token_id.clone(), "token".to_string(), token)
        .with_user(vec![0x10, 0x11], "token".to_string(), User::new(vec![0x10, 0x11]));
    let p = provider(dp);

    let pinned = p
        .resolve(&token_id, &r, &[0x10, 0x11])
        .expect("valid target resolves");

    assert_eq!(pinned.token.id, token_id);
    assert_eq!(pinned.contract.name, "callee");
    assert_eq!(pinned.sender_state.public_key, vec![0x10, 0x11]);
    assert_eq!(pinned.snapshot_ref, r);
}

#[test]
fn provider_fails_closed_unknown_token() {
    let p = provider(StubDataProvider::new());
    let r = SnapshotRef { height: 0, block_hash: vec![] };
    let err = p.resolve(&[0xFF], &r, &[0x10]).expect_err("unknown token");
    assert!(matches!(err, ContractError::InvalidInput(_)), "got {err:?}");
}

#[test]
fn provider_fails_closed_ref_out_of_range() {
    let (token, _r) = chain_token(vec![0xBB], &callee_program());
    let token_id = token.id.clone();
    let dp = StubDataProvider::new()
        .with_token(token_id.clone(), "token".to_string(), token)
        .with_user(vec![0x10, 0x11], "token".to_string(), User::new(vec![0x10, 0x11]));
    let p = provider(dp);
    // Height 99 is beyond the 2-block chain.
    let bad = SnapshotRef { height: 99, block_hash: vec![1] };
    let err = p
        .resolve(&token_id, &bad, &[0x10, 0x11])
        .expect_err("out of range");
    assert!(matches!(err, ContractError::InvalidInput(_)), "got {err:?}");
}

#[test]
fn provider_fails_closed_ref_hash_mismatch() {
    let (token, _r) = chain_token(vec![0xBB], &callee_program());
    let token_id = token.id.clone();
    let dp = StubDataProvider::new()
        .with_token(token_id.clone(), "token".to_string(), token)
        .with_user(vec![0x10, 0x11], "token".to_string(), User::new(vec![0x10, 0x11]));
    let p = provider(dp);
    // Right height, wrong hash.
    let bad = SnapshotRef { height: 1, block_hash: vec![0xDE, 0xAD] };
    let err = p
        .resolve(&token_id, &bad, &[0x10, 0x11])
        .expect_err("hash mismatch");
    assert!(matches!(err, ContractError::InvalidInput(_)), "got {err:?}");
}

#[test]
fn provider_fails_closed_no_contract_asset() {
    // A token with a valid 2-block chain (so the ref validates) but a
    // non-contract asset -> the contract-asset decode must fail closed.
    let mut token = Token::from_asset(&"not-a-contract").expect("token");
    token.id = vec![0xCC];
    token
        .metadata
        .insert("token_type".to_string(), "contract".to_string());
    token
        .metadata
        .insert("contract_engine".to_string(), "Spec".to_string());
    let b0 = det_block(vec![]);
    let b1 = det_block(b0.current_hash.clone());
    let h1 = b1.current_hash.clone();
    token.blockchain.add_block(b0);
    token.blockchain.add_block(b1);
    let r = SnapshotRef { height: 1, block_hash: h1 };

    let dp = StubDataProvider::new()
        .with_token(vec![0xCC], "token".to_string(), token)
        .with_user(vec![0x10, 0x11], "token".to_string(), User::new(vec![0x10, 0x11]));
    let p = provider(dp);

    let err = p
        .resolve(&[0xCC], &r, &[0x10, 0x11])
        .expect_err("no contract asset");
    assert!(matches!(err, ContractError::InvalidInput(_)), "got {err:?}");
}

#[test]
fn provider_fails_closed_missing_user() {
    let (token, r) = chain_token(vec![0xBB], &callee_program());
    let token_id = token.id.clone();
    // Token present, but NO user stored.
    let dp = StubDataProvider::new()
        .with_token(token_id.clone(), "token".to_string(), token);
    let p = provider(dp);

    let err = p
        .resolve(&token_id, &r, &[0x10, 0x11])
        .expect_err("missing user");
    assert!(matches!(err, ContractError::InvalidInput(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Full wiring: a Spec caller's Op::Call resolves its target through the
// executor's provider and folds the callee result.
// ---------------------------------------------------------------------------

/// A Spec caller program that issues a cross-contract call to `target` and
/// emits the call status (1 = success, 0 = failure).
fn call_program(target: Vec<u8>, r: &SnapshotRef) -> InstructionProgram {
    InstructionProgram {
        version: 1,
        ops: vec![
            Op::Call {
                target_token: target,
                entry_point: "ContractCall".to_string(),
                call_payload: vec![7, 8],
                ref_height: r.height,
                ref_hash: r.block_hash.clone(),
            },
            Op::Emit,
        ],
    }
}

#[tokio::test]
async fn executor_call_wiring_resolves_target_and_folds_result() {
    // B (the target): a Spec callee with a 2-block chain.
    let (b_token, b_ref) = chain_token(vec![0xBB], &callee_program());
    let b_id = b_token.id.clone();

    // A (the caller): a Spec contract whose program calls B.
    let a_prog = call_program(b_id.clone(), &b_ref);
    let a_contract = SmartContract {
        name: "caller".to_string(),
        bytecode: spec_bytecode(&a_prog),
        version: "1".to_string(),
        storage: Default::default(),
        owners: vec![],
        threshold: 0,
    };
    let mut a_token = Token::from_asset(&a_contract).expect("caller token");
    a_token.id = vec![0xAA];
    a_token
        .metadata
        .insert("token_type".to_string(), "contract".to_string());
    a_token
        .metadata
        .insert("contract_engine".to_string(), "Spec".to_string());

    let sender = vec![0x10, 0x11];
    let dp: Arc<dyn DataProvider> = Arc::new(
        StubDataProvider::new()
            .with_token(a_token.id.clone(), "token".to_string(), a_token)
            .with_token(b_id.clone(), "token".to_string(), b_token)
            .with_user(sender.clone(), "token".to_string(), User::new(sender.clone())),
    );

    // A pending registry with a tx that targets A (token [0xAA]).
    let pending_registry = Arc::new(PendingTransactionRegistry::new());
    let tx = Transaction {
        payload: vec![],
        gas_limit: 0,
        id: "xcall_tx_001".to_string(),
        action: "ContractCall".to_string(),
        token_id: vec![0xAA],
        bid: None,
        sequence_number: 1,
        sender: sender.clone(),
        receiver: vec![],
        amount: None,
        timestamp: 1_700_000_000,
        result_hash: vec![],
        sender_signature: vec![],
        result_data: vec![],
    };
    let pending = PendingTransaction::new(
        "xcall_tx_001".to_string(),
        TransactionState::Preloaded { transaction: tx },
    );
    pending_registry
        .add_transaction("xcall_tx_001".to_string(), pending)
        .unwrap();

    // The executor dispatches only with at least one finalizer registered.
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

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        Arc::new(NodeIdentity::generate_in_memory()),
        node_registry,
        dp,
        pending_registry,
        make_test_hash_provider(),
        100,
        "token".to_string(),
        make_test_engine_registry(),
    );

    executor
        .preload_for_transaction("xcall_tx_001")
        .await
        .expect("preload should be admitted");

    let outcome = wait_for_settle(&executor, "xcall_tx_001")
        .await
        .expect("task should settle");
    let result = outcome.expect("the cross-contract call should succeed");

    // The caller emitted the call status: 1 (success) as a little-endian u64.
    let status = u64::from_le_bytes(result.result_data.as_slice()[..8].try_into().unwrap());
    assert_eq!(status, 1, "the cross-contract call must resolve and succeed");
}
