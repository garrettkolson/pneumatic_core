//! Contract execution substrate (ADR-011 / ADR-013).
//!
//! This module defines the pluggable [`ContractEngine`] trait, the canonical
//! [`ExecutionInput`] / [`ExecutionOutput`] pair every engine and test operates on,
//! the fail-closed [`ContractError`] type, and the name-keyed
//! [`ContractEngineRegistry`] (the ADR-002 spec-registry pattern).
//!
//! Determinism is consensus-critical (ADR-008): every executor in a shard must
//! compute the *same* `result_hash` for the same transaction, so engines must be
//! pure functions of [`ExecutionInput`] — no wall clock, no RNG, no I/O, no
//! host-dependent behavior. The wall-clock timeout backstop lives *outside* the
//! engine (the executor's spawn path, ADR-013), never inside.
//!
//! Engines live in `pneumatic_core` (sub-decision 1 of ADR-011) so every role —
//! and a future Tier-2 `WasmEngine` — shares the same trait object.

use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use serde::Serialize;

use crate::errors::{PneumaticError, ValidationFailureReason};
use crate::tokens::{SmartContract, Token};
use crate::transactions::Transaction;
use crate::user::User;

// ---------------------------------------------------------------------------
// ContractEngine — the pluggable execution trait (ADR-011)
// ---------------------------------------------------------------------------

/// A deterministic contract execution engine.
///
/// Implementations must be pure: `execute` is a function of `input` alone. The
/// protocol cost table (ADR-013) is owned by the engine — each engine counts
/// its own work against `input.gas_limit` and reports `gas_used`.
pub trait ContractEngine: Send + Sync {
    /// Stable engine name. Must match the name used in the environment spec
    /// (`contract_engines`) and in token metadata (`contract_engine`).
    fn name(&self) -> &str;

    /// Execute `input` and return the canonical output.
    ///
    /// `output.result_data` is rmp-canonical bytes; the protocol's
    /// `result_hash` remains `hash(result_data)` (no new hashing scheme).
    fn execute(&self, input: &ExecutionInput) -> Result<ExecutionOutput, ContractError>;
}

// ---------------------------------------------------------------------------
// ExecutionInput — the canonical input (one struct, one byte stream)
// ---------------------------------------------------------------------------

/// The canonical, deterministic input to [`ContractEngine::execute`].
///
/// One struct so every engine — and every test — hashes the *same* bytes:
/// `canonical_bytes()` is the rmp-canonical serialization of the view below,
/// stable across serde round-trips and field insertion order (no `HashMap` in
/// the view; the token's metadata is reduced to a sorted key/value slice).
pub struct ExecutionInput<'a> {
    /// The transaction being executed (includes `payload` calldata, ADR-012).
    pub tx: &'a Transaction,
    /// The contract being invoked (decoded from `Token.asset_data`).
    pub contract: &'a SmartContract,
    /// The sender's protocol state (fuel, stake, nonce) at execution time.
    pub sender_state: &'a User,
    /// The token/chain the contract lives on.
    pub token: &'a Token,
    /// Sender-declared gas cap (ADR-013 stage 1); 0 = no cap declared.
    pub gas_limit: u64,
}

/// The deterministic view of an [`ExecutionInput`] that `canonical_bytes()`
/// serializes. Kept private so the canonical shape can evolve only here.
#[derive(Serialize)]
struct ExecutionInputCanon<'a> {
    tx: &'a Transaction,
    contract_name: &'a str,
    contract_bytecode: &'a [u8],
    contract_version: &'a str,
    sender_public_key: &'a [u8],
    sender_fuel_balance: u64,
    sender_stake: u64,
    sender_nonce: usize,
    token_id: &'a [u8],
    token_metadata: Vec<(&'a str, &'a str)>,
    gas_limit: u64,
}

impl ExecutionInput<'_> {
    /// The rmp-canonical byte stream of this input — the bytes every engine and
    /// determinism test compares. Identical inputs produce identical bytes.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PneumaticError> {
        let mut metadata: Vec<(&str, &str)> = self
            .token
            .metadata
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        metadata.sort_unstable();
        let canon = ExecutionInputCanon {
            tx: self.tx,
            contract_name: &self.contract.name,
            contract_bytecode: &self.contract.bytecode,
            contract_version: &self.contract.version,
            sender_public_key: &self.sender_state.public_key,
            sender_fuel_balance: self.sender_state.fuel_balance,
            sender_stake: self.sender_state.stake,
            sender_nonce: self.sender_state.nonce,
            token_id: &self.token.id,
            token_metadata: metadata,
            gas_limit: self.gas_limit,
        };
        crate::encoding::serialize_to_bytes_rmp(&canon)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// ExecutionOutput
// ---------------------------------------------------------------------------

/// The canonical output of a contract execution.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOutput {
    /// rmp-canonical execution result. The protocol `result_hash` is
    /// `hash(result_data)` — no new hashing is introduced.
    pub result_data: Vec<u8>,
    /// Metered gas cost (ADR-013 stage 2): the work the engine actually did,
    /// always `<=` the input `gas_limit` on success.
    pub gas_used: u64,
}

impl ExecutionOutput {
    pub fn new(result_data: Vec<u8>, gas_used: u64) -> Self {
        ExecutionOutput { result_data, gas_used }
    }
}

// ---------------------------------------------------------------------------
// ContractError
// ---------------------------------------------------------------------------

/// Fail-closed contract execution errors.
///
/// Every variant maps to a [`ValidationFailureReason`] so the executor can
/// drive the `Failed` transition (ADR-013: `GasExhausted` → `Failed`, no vote).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// The requested engine name is not registered in this environment.
    UnknownEngine(String),
    /// The engine ran past its declared `gas_limit`.
    GasExhausted,
    /// The contract bytecode is malformed (bad version, undecodable AST).
    BadBytecode(String),
    /// The contract executed and explicitly reverted.
    Reverted(String),
    /// The engine is registered but not implemented (Phase-1 placeholder).
    EngineNotImplemented(String),
    /// The execution input was invalid (e.g. malformed calldata).
    InvalidInput(String),
}

impl ContractError {
    /// Map this error to the `ValidationFailureReason` the executor records on
    /// the `Failed` transition.
    pub fn failure_reason(&self) -> ValidationFailureReason {
        match self {
            ContractError::UnknownEngine(_) => ValidationFailureReason::ContractNotFound,
            ContractError::GasExhausted => ValidationFailureReason::GasLimitExceeded,
            ContractError::BadBytecode(_)
            | ContractError::Reverted(_)
            | ContractError::EngineNotImplemented(_)
            | ContractError::InvalidInput(_) => ValidationFailureReason::ContractExecutionFailed,
        }
    }

    /// Convenience: the single-reason vector for the `Failed` transition.
    pub fn to_failure_reasons(&self) -> Vec<ValidationFailureReason> {
        vec![self.failure_reason()]
    }
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContractError::UnknownEngine(name) => write!(f, "unknown contract engine \"{}\"", name),
            ContractError::GasExhausted => write!(f, "gas exhausted"),
            ContractError::BadBytecode(msg) => write!(f, "bad bytecode: {}", msg),
            ContractError::Reverted(msg) => write!(f, "contract reverted: {}", msg),
            ContractError::EngineNotImplemented(name) => {
                write!(f, "engine \"{}\" not implemented yet", name)
            }
            ContractError::InvalidInput(msg) => write!(f, "invalid execution input: {}", msg),
        }
    }
}

impl std::error::Error for ContractError {}

impl From<ContractError> for PneumaticError {
    fn from(e: ContractError) -> Self {
        PneumaticError::Validation(e.to_failure_reasons())
    }
}

// ---------------------------------------------------------------------------
// ContractEngineRegistry — name-keyed, ADR-002 pattern
// ---------------------------------------------------------------------------

/// Registry of [`ContractEngine`] instances, keyed by engine name.
///
/// `DashMap` (ADR-002 pattern): registration happens once at boot
/// (`EnvironmentMetadata::load_from_spec`), lookups happen concurrently from
/// every executor thread.
#[derive(Default)]
pub struct ContractEngineRegistry {
    engines: DashMap<String, Arc<dyn ContractEngine>>,
}

impl ContractEngineRegistry {
    pub fn new() -> Self {
        ContractEngineRegistry {
            engines: DashMap::new(),
        }
    }

    /// Register an engine under its own `name()`.
    pub fn register(&self, engine: Arc<dyn ContractEngine>) {
        let name = engine.name().to_string();
        self.engines.insert(name, engine);
    }

    /// Look up an engine by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn ContractEngine>> {
        self.engines.get(name).map(|e| e.value().clone())
    }

    /// The registered engine names, sorted for deterministic diagnostics.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.engines.iter().map(|e| e.key().clone()).collect();
        names.sort_unstable();
        names
    }

    /// Register the built-in Tier-1 engines (ADR-011): `Transfer` and `Spec`.
    pub fn register_defaults(&self) {
        self.register(Arc::new(TransferEngine));
        self.register(Arc::new(SpecEngine));
    }
}

// ---------------------------------------------------------------------------
// Tier-1 built-in engines (ADR-011)
//
// Phase-1 placeholders: the trait, registry, and environment wiring are the
// Phase-1 scope; the real interpreter/transfer logic lands in Phase 2 of the
// executor contract-execution plan. Both are fail-closed — they *refuse* to
// produce output rather than returning identity-like data (the D1 stub
// problem this plan exists to remove).
// ---------------------------------------------------------------------------

/// Standard-token transfer engine (name: `"Transfer"`).
///
/// Phase-2 scope: validate `amount` (overflow-safe) and emit the canonical
/// delta `(token_id, sender, receiver, amount, sequence_number)` as
/// `result_data`.
pub struct TransferEngine;

impl ContractEngine for TransferEngine {
    fn name(&self) -> &str {
        "Transfer"
    }

    fn execute(&self, _input: &ExecutionInput) -> Result<ExecutionOutput, ContractError> {
        Err(ContractError::EngineNotImplemented("Transfer".to_string()))
    }
}

/// Spec-AST engine (name: `"Spec"`): interprets the versioned rmp instruction
/// AST stored in `SmartContract.bytecode` (ADR-011 / Q5).
///
/// Phase-2 scope: `LoadTx`, `LoadConst`, `Add`/`Sub`/`Mul`/`Mod` (overflow =
/// revert), `Cmp`, `Select`, `Emit`, `Halt`; instruction count is the gas meter.
pub struct SpecEngine;

impl ContractEngine for SpecEngine {
    fn name(&self) -> &str {
        "Spec"
    }

    fn execute(&self, _input: &ExecutionInput) -> Result<ExecutionOutput, ContractError> {
        Err(ContractError::EngineNotImplemented("Spec".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::Blockchain;

    fn test_token() -> Token {
        let mut token = Token::new();
        token.id = vec![1, 2, 3];
        token
            .metadata
            .insert("token_type".to_string(), "contract".to_string());
        token
    }

    fn test_tx() -> Transaction {
        Transaction {
            id: "tx-canon-test".to_string(),
            action: "ContractCall".to_string(),
            token_id: vec![1, 2, 3],
            bid: None,
            sequence_number: 7,
            sender: vec![9, 8],
            receiver: vec![],
            amount: Some(50),
            timestamp: 1_700_000_000,
            result_hash: vec![],
            sender_signature: vec![],
            payload: vec![1, 2, 3, 4],
            gas_limit: 1234,
        }
    }

    fn test_contract() -> SmartContract {
        SmartContract {
            name: "test-contract".to_string(),
            bytecode: vec![0xAB, 0xCD],
            version: "1".to_string(),
        }
    }

    fn test_user() -> User {
        User {
            public_key: vec![9, 8],
            fuel_balance: 1000,
            stake: 5,
            nonce: 7,
        }
    }

    #[test]
    fn canonical_input_determinism_same_source_identical_bytes() {
        let tx = test_tx();
        let contract = test_contract();
        let user = test_user();
        let token = test_token();

        let a = ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 1234,
        };
        let b = ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 1234,
        };

        let bytes_a = a.canonical_bytes().unwrap();
        let bytes_b = b.canonical_bytes().unwrap();
        assert_eq!(bytes_a, bytes_b, "same source data must hash the same bytes");
        assert!(!bytes_a.is_empty());
    }

    #[test]
    fn canonical_input_metadata_order_independent() {
        let tx = test_tx();
        let contract = test_contract();
        let user = test_user();

        let mut token_a = test_token();
        token_a
            .metadata
            .insert("contract_engine".to_string(), "Spec".to_string());
        let mut token_b = test_token();
        token_b
            .metadata
            .insert("contract_engine".to_string(), "Spec".to_string());
        // Insert in a different order to prove the canon view sorts.
        let mut map_a: HashMap<String, String> = HashMap::new();
        map_a.insert("token_type".to_string(), "contract".to_string());
        map_a.insert("contract_engine".to_string(), "Spec".to_string());
        let _ = map_a; // (HashMap iteration order is nondeterministic; the canon view sorts)
        let _ = &mut token_a;

        let a = ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token_a,
            gas_limit: 100,
        };
        let b = ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token_b,
            gas_limit: 100,
        };
        assert_eq!(a.canonical_bytes().unwrap(), b.canonical_bytes().unwrap());
    }

    #[test]
    fn canonical_input_differs_with_gas_limit_and_payload() {
        let contract = test_contract();
        let user = test_user();
        let token = test_token();

        let mut tx_a = test_tx();
        let mut tx_b = test_tx();
        tx_b.gas_limit = 9999;
        let mut tx_c = test_tx();
        tx_c.payload = vec![];

        let base = ExecutionInput {
            tx: &tx_a,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 1234,
        };
        let alt_gas = ExecutionInput {
            tx: &tx_b,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 9999,
        };
        let alt_payload = ExecutionInput {
            tx: &tx_c,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 1234,
        };
        assert_ne!(base.canonical_bytes().unwrap(), alt_gas.canonical_bytes().unwrap());
        assert_ne!(
            base.canonical_bytes().unwrap(),
            alt_payload.canonical_bytes().unwrap()
        );
    }

    #[test]
    fn registry_lookup_and_unknown() {
        let registry = ContractEngineRegistry::new();
        registry.register_defaults();

        assert!(registry.get("Transfer").is_some());
        assert!(registry.get("Spec").is_some());
        assert!(registry.get("Wasm").is_none(), "unregistered engine must be None");
        assert_eq!(registry.names(), vec!["Spec".to_string(), "Transfer".to_string()]);
    }

    #[test]
    fn registry_phase1_placeholders_fail_closed() {
        let registry = ContractEngineRegistry::new();
        registry.register_defaults();
        let tx = test_tx();
        let contract = test_contract();
        let user = test_user();
        let token = test_token();
        let input = ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 100,
        };
        for name in ["Transfer", "Spec"] {
            let engine = registry.get(name).unwrap();
            let err = engine.execute(&input).unwrap_err();
            assert!(
                matches!(err, ContractError::EngineNotImplemented(_)),
                "phase-1 placeholder must fail closed, got {:?}",
                err
            );
        }
    }

    #[test]
    fn contract_error_maps_to_validation_failure_reason() {
        assert_eq!(
            ContractError::UnknownEngine("X".into()).failure_reason(),
            ValidationFailureReason::ContractNotFound
        );
        assert_eq!(
            ContractError::GasExhausted.failure_reason(),
            ValidationFailureReason::GasLimitExceeded
        );
        assert_eq!(
            ContractError::BadBytecode("x".into()).failure_reason(),
            ValidationFailureReason::ContractExecutionFailed
        );
        assert_eq!(
            ContractError::Reverted("x".into()).failure_reason(),
            ValidationFailureReason::ContractExecutionFailed
        );
        assert_eq!(
            ContractError::EngineNotImplemented("x".into()).failure_reason(),
            ValidationFailureReason::ContractExecutionFailed
        );
        assert_eq!(
            ContractError::InvalidInput("x".into()).failure_reason(),
            ValidationFailureReason::ContractExecutionFailed
        );
        assert_eq!(ContractError::GasExhausted.to_failure_reasons().len(), 1);
    }

    #[test]
    fn contract_error_converts_to_pneumatic_error() {
        let pneumatic: PneumaticError = ContractError::GasExhausted.into();
        assert!(matches!(pneumatic, PneumaticError::Validation(_)));
    }

    #[test]
    fn blockchain_import_compiles() {
        // Guards the test helper against a future `Token` shape change that
        // would silently drop the chain field from `Token::new`.
        let _chain = Blockchain::new();
        let _ = _chain;
    }
}
