//! P10 exit criterion — cross-executor determinism (ADR-008).
//!
//! Two independent [`Executor`] instances fed the *same shard inputs* (the same
//! identity, data provider, engine registry, hash provider, and transaction)
//! must produce **identical** execution: the same `result_hash`, and "Sign"
//! votes that are interchangeable — the same `transaction_hash`, both of them
//! valid signatures over that hash under the executor's identity.
//!
//! This is what lets any two honest executors in a shard agree on a block's
//! `result_hash` without trusting each other: the engine is a pure function of
//! the pinned inputs, so the `result_hash` (the hash of the engine output) is
//! reproducible, and the "Sign" vote authenticates that hash under the
//! executor's identity.
//!
//! **Why the signature is verified, not byte-compared.** The "Sign" vote's
//! `signature` is a hybrid `(Ed25519 · ML-DSA-44)` value. The ML-DSA-44 half
//! (FIPS 203) is *non-deterministic by design* — each signing draws a fresh
//! random nonce — so two signatures over the same message under the same key
//! are **not** byte-identical. The deterministic, comparable invariant is the
//! `result_hash`; the signature is checked for validity (both halves verify
//! under the identity's public key) rather than byte equality.
//!
//! Two shapes are covered, per the plan:
//! - a **calling tx** (a `ContractCall` against a `Spec` contract);
//! - a **Wasm module** (a `ContractCall` against a `Wasm` contract, so the
//!   `WasmEngine` — module instantiation + execution — is on the path).

use super::helpers::*;
use super::super::*;

use std::sync::Arc;
use std::time::Duration;

use pneumatic_core::contracts::{ContractEngineRegistry, InstructionProgram, Op, WasmEngine};
use pneumatic_core::data::StubDataProvider;
use pneumatic_core::transactions::PendingTransaction;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A `Spec` contract token whose program is `7 + 35 → Emit` (deterministic
/// 42). Mirrors the e2e `contract_token` + `contract_asset` builders.
fn spec_token(id: Vec<u8>) -> Token {
    let program = InstructionProgram {
        version: 1,
        ops: vec![Op::LoadConst(7), Op::LoadConst(35), Op::Add, Op::Emit],
    };
    let bytecode = serialize_to_bytes_rmp(&program).expect("spec program serializes");
    let contract = SmartContract {
        name: "spec-adder".to_string(),
        bytecode,
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
    token
}

/// A `Wasm` contract token carrying the `wasm_sum` module (sums its canonical
/// input bytes and emits a LE `u32`; no storage delta).
fn wasm_token(id: Vec<u8>) -> Token {
    let wasm_bytes = include_bytes!("../../../../src/contracts/wasm_fixtures/wasm_sum.wasm");
    let contract = SmartContract {
        name: "wasm-sum".to_string(),
        bytecode: wasm_bytes.to_vec(),
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
        .insert("contract_engine".to_string(), "Wasm".to_string());
    token
}

/// A deterministic sender account (the `tx.sender` key + `sender_signature`).
fn sender() -> (Ed25519Provider, Vec<u8>) {
    let account = Ed25519Provider::from_seed([0x42u8; 32]);
    let pk = account.public_key().expect("ed25519 public key");
    (account, pk)
}

/// The sender's `User` record (ample fuel, `nonce == 1`).
fn user(pk: &Vec<u8>) -> User {
    User {
        public_key: pk.clone(),
        fuel_balance: 1_000_000,
        stake: 0,
        nonce: 1,
    }
}

/// A sender-signed `ContractCall` transaction with an empty calldata payload.
fn call_tx(
    account: &Ed25519Provider,
    sender: &Vec<u8>,
    id: &str,
    token_id: &Vec<u8>,
) -> Transaction {
    let mut tx = Transaction {
        id: id.to_string(),
        action: "ContractCall".to_string(),
        token_id: token_id.clone(),
        bid: None,
        sequence_number: 1,
        sender: sender.clone(),
        receiver: vec![],
        amount: Some(10),
        timestamp: 1_700_000_000,
        result_hash: vec![],
        sender_signature: vec![],
        payload: vec![],
        gas_limit: 0,
        result_data: vec![],
    };
    let canon = tx.canonical_signature_bytes().expect("canonical signature bytes");
    tx.sender_signature = account.sign_data(&canon).expect("sender signs the tx");
    tx
}

/// A `StubDataProvider` serving the given token + sender user under partition
/// `"token"` (the executor's `partition_id`).
fn provider(token: &Token, sender_pk: &Vec<u8>) -> Arc<dyn DataProvider> {
    Arc::new(
        StubDataProvider::new()
            .with_token(token.id.clone(), "token".to_string(), token.clone())
            .with_user(sender_pk.clone(), "token".to_string(), user(sender_pk)),
    )
}

/// Engine registry with the Tier-1 defaults (`Transfer`, `Spec`) plus the
/// opt-in `Wasm` engine (the `Wasm` token needs it registered).
fn engines() -> Arc<ContractEngineRegistry> {
    let registry = Arc::new(ContractEngineRegistry::new());
    registry.register_defaults();
    registry.register(Arc::new(WasmEngine));
    registry
}

/// Register `tx` in a fresh pending registry in the `Preloaded` state (the
/// state the executor's `run_execution` expects).
fn registry_with(tx: &Transaction) -> Arc<PendingTransactionRegistry> {
    let registry = Arc::new(PendingTransactionRegistry::new());
    let pending = PendingTransaction::new(
        tx.id.clone(),
        TransactionState::Preloaded {
            transaction: tx.clone(),
        },
    );
    let _ = registry.add_transaction(tx.id.clone(), pending);
    registry
}

/// Build an [`Executor`] wired to a `RecordingConnection` finalizer peer so its
/// outgoing "Sign" vote is captured. Shares `identity` / `data_provider` /
/// `engines` / `hash_provider` with its twin — the *same shard inputs* — but
/// owns its own pending + node registries (independent instances).
fn build_executor(
    identity: &Arc<NodeIdentity>,
    data_provider: Arc<dyn DataProvider>,
    engine_registry: Arc<ContractEngineRegistry>,
    hash_provider: Arc<dyn HashProvider>,
    tx: &Transaction,
    recorder: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
) -> Executor {
    let node_registry = make_test_node_registry();
    assert!(node_registry.register_peer(
        vec![0xAB; 32],
        [5u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(RecordingConnection { recorder }),
    ));
    Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        identity.clone(),
        node_registry,
        data_provider,
        registry_with(tx),
        hash_provider,
        10,
        "token".to_string(),
        engine_registry,
    )
    .with_execution_timeout(Duration::from_secs(60))
}

/// Pull the "Sign" vote out of a recorder (the `TransactionSignature` the
/// finalizer receives). Panics if no "Sign" was captured.
fn extract_sign_vote(
    captured: &[Vec<u8>],
) -> pneumatic_core::transactions::TransactionSignature {
    for bytes in captured {
        let message: pneumatic_core::messages::Message =
            deserialize_rmp_to(bytes).expect("captured payload is a Message");
        if message.action == "Sign" {
            return deserialize_rmp_to(&message.body)
                .expect("Sign body is a TransactionSignature");
        }
    }
    panic!("no 'Sign' vote captured from the finalizer peer");
}

/// Assert the two "Sign" votes are interchangeable: same `result_hash`, same
/// non-cryptographic fields, and both signatures valid under `identity`.
fn assert_votes_agree(
    identity: &Arc<NodeIdentity>,
    vote1: &pneumatic_core::transactions::TransactionSignature,
    vote2: &pneumatic_core::transactions::TransactionSignature,
) {
    // Identical result_hash (the deterministic invariant the block commits on).
    assert!(
        !vote1.transaction_hash.is_empty(),
        "the 'Sign' vote must carry a non-empty result_hash"
    );
    assert_eq!(
        vote1.transaction_hash, vote2.transaction_hash,
        "the result_hash must be identical across executors"
    );

    // Identical non-cryptographic vote fields.
    assert_eq!(vote1.transaction_id, vote2.transaction_id, "same transaction");
    assert_eq!(vote1.env_id, vote2.env_id, "same env");
    assert_eq!(vote1.current_stake, vote2.current_stake, "same stake");

    // Both signatures are valid hybrid `(Ed25519 · ML-DSA-44)` signatures over
    // the `result_hash` under the executor's identity (the ML-DSA half is
    // non-deterministic, so we verify rather than byte-compare).
    let pk = identity
        .ed25519
        .public_key()
        .expect("identity has a public key");
    assert!(
        identity
            .ed25519
            .check_signature(&vote1.signature, &pk, &vote1.transaction_hash)
            .expect("vote 1 signature verifies"),
        "vote 1's signature must verify under the executor identity"
    );
    assert!(
        identity
            .ed25519
            .check_signature(&vote2.signature, &pk, &vote2.transaction_hash)
            .expect("vote 2 signature verifies"),
        "vote 2's signature must verify under the executor identity"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A calling tx (a `Spec` `ContractCall`) executed by two independent executors
/// yields an identical `result_hash` and interchangeable "Sign" votes.
#[tokio::test]
async fn determinism_calling_tx_identical_across_two_executors() {
    let identity = Arc::new(NodeIdentity::generate_in_memory());
    let token = spec_token(vec![0x0B]);
    let token_id = token.id.clone();
    let (account, sender_pk) = sender();
    let data_provider = provider(&token, &sender_pk);
    let engine_registry = engines();
    let hash_provider = make_test_hash_provider();

    let tx = call_tx(&account, &sender_pk, "det_spec_1", &token_id);

    let rec1 = Arc::new(std::sync::Mutex::new(Vec::new()));
    let rec2 = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ex1 = build_executor(
        &identity,
        data_provider.clone(),
        engine_registry.clone(),
        hash_provider.clone(),
        &tx,
        rec1.clone(),
    );
    let ex2 = build_executor(
        &identity,
        data_provider,
        engine_registry,
        hash_provider,
        &tx,
        rec2.clone(),
    );

    ex1.preload_for_transaction("det_spec_1")
        .await
        .expect("executor 1 begins execution");
    ex2.preload_for_transaction("det_spec_1")
        .await
        .expect("executor 2 begins execution");

    let r1 = wait_for_settle(&ex1, "det_spec_1")
        .await
        .expect("executor 1 settles");
    let r2 = wait_for_settle(&ex2, "det_spec_1")
        .await
        .expect("executor 2 settles");
    assert!(r1.is_ok(), "executor 1 must succeed: {:?}", r1);
    assert!(r2.is_ok(), "executor 2 must succeed: {:?}", r2);

    let vote1 = extract_sign_vote(&rec1.lock().unwrap().clone());
    let vote2 = extract_sign_vote(&rec2.lock().unwrap().clone());
    assert_votes_agree(&identity, &vote1, &vote2);
}

/// A Wasm module (a `Wasm` `ContractCall`) executed by two independent executors
/// yields an identical `result_hash` and interchangeable "Sign" votes — the
/// `WasmEngine` (module instantiation + execution) is on the path.
#[tokio::test]
async fn determinism_wasm_module_identical_across_two_executors() {
    let identity = Arc::new(NodeIdentity::generate_in_memory());
    let token = wasm_token(vec![0x0C]);
    let token_id = token.id.clone();
    let (account, sender_pk) = sender();
    let data_provider = provider(&token, &sender_pk);
    let engine_registry = engines();
    let hash_provider = make_test_hash_provider();

    let tx = call_tx(&account, &sender_pk, "det_wasm_1", &token_id);

    let rec1 = Arc::new(std::sync::Mutex::new(Vec::new()));
    let rec2 = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ex1 = build_executor(
        &identity,
        data_provider.clone(),
        engine_registry.clone(),
        hash_provider.clone(),
        &tx,
        rec1.clone(),
    );
    let ex2 = build_executor(
        &identity,
        data_provider,
        engine_registry,
        hash_provider,
        &tx,
        rec2.clone(),
    );

    ex1.preload_for_transaction("det_wasm_1")
        .await
        .expect("executor 1 begins execution");
    ex2.preload_for_transaction("det_wasm_1")
        .await
        .expect("executor 2 begins execution");

    let r1 = wait_for_settle(&ex1, "det_wasm_1")
        .await
        .expect("executor 1 settles");
    let r2 = wait_for_settle(&ex2, "det_wasm_1")
        .await
        .expect("executor 2 settles");
    assert!(r1.is_ok(), "executor 1 must succeed: {:?}", r1);
    assert!(r2.is_ok(), "executor 2 must succeed: {:?}", r2);

    let vote1 = extract_sign_vote(&rec1.lock().unwrap().clone());
    let vote2 = extract_sign_vote(&rec2.lock().unwrap().clone());
    assert_votes_agree(&identity, &vote1, &vote2);
}
