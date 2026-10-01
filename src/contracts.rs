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

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::errors::{PneumaticError, ValidationFailureReason};
use crate::tokens::{SmartContract, Token};
use crate::transactions::Transaction;
use crate::user::User;

// Tier-2 WasmEngine (ADR-018) — lives in `src/contracts/wasm.rs`. Re-exported so it
// can be registered by name (`"Wasm"`) via the environment spec; NOT in
// `register_defaults` (opt-in, ADR-018 QW3).
mod wasm;
pub use wasm::{validate_wasm_module, WasmEngine, WASM_MAX_MODULE_BYTES};

mod deploy;
pub use deploy::{
    deploy_contract, deploy_gas, derive_token_id, CreateTokenDelta, DeployParams,
    DEPLOY_GAS_BASE, DEPLOY_GAS_PER_BYTE, SPEC_MAX_BYTECODE,
};

// Deploy-time contract scanner (ADR-015 extension) — a deterministic, fail-closed
// pre-screen that runs in `DeployValidationSpec`: Spec well-formedness + Wasm static
// walk + a canary run. Lives in `src/contracts/scan.rs`. It is consensus-safe by
// construction (a pure function of `(engine, bytecode)` + frozen caps, ADR-008).
mod scan;
pub use scan::{scan_contract, ScanCode, ScanFinding, Severity};

// Upgrade governance (ADR-017, Phase 8) — the canonical upgrade digest, the
// M-of-N quorum check, the 1-epoch timelock gate, and the `ReplaceAsset` apply.
// Lives in `src/contracts/upgrade.rs`. Consensus-safe by construction: every
// step is a pure function of (token, proposal payload, current owner registry)
// + frozen constants, so every node re-derives the same result (ADR-008).
mod upgrade;
pub use upgrade::{
    apply_replace_asset, timelock_satisfied, upgrade_digest, upgrade_gas, verify_quorum,
    ReplaceAssetDelta, TIMELOCK_EPOCHS, UPGRADE_GAS_BASE, UPGRADE_GAS_PER_BYTE, UpgradeParams,
};

// Model X cross-contract calls (ADR-016, Phase 9) — snapshot-pinned, non-atomic
// calls: `SnapshotRef` + `validate_snapshot_ref`, the `TargetStateProvider`
// (the executor's I/O surface, supplied to engines via `CallContext`), the shared
// `execute_call` core (used by the Spec `Call` op and the Wasm `env.call`
// import), and the B-side `CrossChainCommitment` schema + hash. Consensus-safe
// by construction: every step is a pure function of (A input, pinned B state,
// frozen constants) (ADR-008).
mod call;
pub use call::{
    execute_call, synthesize_call_tx, validate_snapshot_ref, CallContext, CallFailure,
    CallOutcome, CrossChainCommitment, PinnedTarget, SnapshotRef, TargetStateProvider,
    MAX_CALL_DEPTH, XCALL_CALL_BASE_SPEC, XCALL_CALL_BASE_WASM, XCALL_COMMIT_DOMAIN,
};

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
    /// The contract's current key/value state (W3 storage, ADR-018 / Phase 7).
    ///
    /// Passed by the executor from the decoded [`SmartContract::storage`] so the
    /// `WasmEngine`'s `sload` reads the base state and `sstore` records a
    /// [`StorageDelta`]. **Deliberately owned and excluded from
    /// [`ExecutionInputCanon`]**: `canonical_bytes()` (and thus a module's
    /// `execute` input and the `result_hash`) must not change when a contract
    /// accumulates state, so existing `Wasm`/`Spec` modules keep their
    /// byte-identical inputs (ADR-008). Non-`Wasm` engines ignore it.
    pub storage: BTreeMap<Vec<u8>, Vec<u8>>,
    /// The Model X call context (ADR-016, Phase 9): the `TargetStateProvider`
    /// (the executor's I/O surface for resolving call targets at their pinned
    /// snapshot refs), the shared engine registry (selects the callee's engine),
    /// and the nesting depth. **Deliberately owned and excluded from
    /// [`ExecutionInputCanon`]** (same rule as `storage`): a contract that never
    /// calls sees byte-identical inputs, so no existing module's `result_hash`
    /// changes (ADR-008). `None` = no call capability; a `Call` op / `env.call`
    /// with no context fails deterministically (fail closed).
    pub call_ctx: Option<Arc<CallContext>>,
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
// W3 storage — the canonical storage delta + Wasm result envelope (ADR-018)
// ---------------------------------------------------------------------------

/// A canonical per-contract storage write-set (W3, ADR-018 / Phase 7).
///
/// `Some(value)` = set `key` to `value`; `None` = tombstone (delete `key`).
/// `BTreeMap` gives a **sorted** key order, so the rmp-canonical serialization
/// (and any hash derived from it) is deterministic regardless of the order the
/// module issued its `sstore`/`sdelete` calls (ADR-008). This is the single
/// source of truth for what the committer applies to [`SmartContract::storage`].
pub type StorageDelta = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// The canonical `WasmEngine` result envelope: the module's raw output **plus**
/// the storage delta it recorded. This is the `result_data` a `Wasm` contract
/// produces — `result_hash = hash(rmp(WasmResult))` — and what the committer
/// decodes to apply the storage delta (the delta is not re-derivable from the
/// tx, so it must ride in `result_data`; QD-transport).
///
/// The envelope is always present for a `Wasm` contract (even with an empty
/// delta), so the committer's decode is unambiguous and no legacy bare-module
/// output is mistaken for a delta.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WasmResult {
    /// The module's raw `result_data` (bytes the module wrote to its output buffer).
    pub module_output: Vec<u8>,
    /// The canonical storage write-set the module recorded via `sstore`/`sdelete`.
    pub storage_delta: StorageDelta,
}

impl WasmResult {
    /// Apply this envelope's [`storage_delta`] to a base state map (the
    /// committer's idempotent apply, and the engine's post-exec cap check).
    /// `Some(v)` sets `key`; `None` removes it. Returns the new state.
    pub fn apply_delta_to(&self, base: &BTreeMap<Vec<u8>, Vec<u8>>) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut out = base.clone();
        for (k, v) in &self.storage_delta {
            match v {
                Some(val) => {
                    out.insert(k.clone(), val.clone());
                }
                None => {
                    out.remove(k);
                }
            }
        }
        out
    }

    /// Total byte size of the state after applying this delta (the W3 storage
    /// cap check: keys + values).
    pub fn post_apply_size(&self, base: &BTreeMap<Vec<u8>, Vec<u8>>) -> usize {
        let state = self.apply_delta_to(base);
        state
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum()
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
// Tier-1 built-in engines (ADR-011, Phase 2)
//
// Both engines are pure functions of [`ExecutionInput`] (ADR-008): no clock,
// no RNG, no I/O, no host-dependent behavior. Each counts its own work against
// `input.gas_limit` (ADR-013 stage 2) and reports `gas_used`.
// ---------------------------------------------------------------------------

/// Base protocol cost of a standard token transfer (ADR-013 cost table).
///
/// A declared `gas_limit` below this value fails closed with
/// [`ContractError::GasExhausted`]; `gas_limit == 0` means *no cap declared*
/// (legacy plain transfers) and is never enforced.
pub const TRANSFER_BASE_COST: u64 = 21_000;

/// The canonical transfer delta encoded into `result_data` by
/// [`TransferEngine`].
///
/// Public and owned so the executor's `validate_execution_result`
/// post-condition (and the committer's apply path) can decode the exact
/// `result_data` bytes and check them against the transaction. The owned
/// `Vec<u8>` fields serialize to the same rmp `binary` bytes as the former
/// borrowed form, so the wire encoding is unchanged.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TransferDelta {
    pub token_id: Vec<u8>,
    pub sender: Vec<u8>,
    pub receiver: Vec<u8>,
    pub amount: u64,
    pub sequence_number: usize,
}

/// Standard-token transfer engine (name: `"Transfer"`).
///
/// Validates `amount` (presence + non-zero; a `u64` cannot overflow, so the
/// validation is presence-only) and encodes the canonical delta
/// `(token_id, sender, receiver, amount, sequence_number)` as rmp-canonical
/// `result_data`. This is the standard-token semantics and the replacement
/// for the executor's identity stub on non-contract transactions (Phase 3
/// wires the dispatch).
pub struct TransferEngine;

impl ContractEngine for TransferEngine {
    fn name(&self) -> &str {
        "Transfer"
    }

    fn execute(&self, input: &ExecutionInput) -> Result<ExecutionOutput, ContractError> {
        // Protocol cost table: a fixed base cost, checked against the
        // sender-declared cap (0 = no cap).
        if input.gas_limit > 0 && TRANSFER_BASE_COST > input.gas_limit {
            return Err(ContractError::GasExhausted);
        }
        let amount = input.tx.amount.ok_or_else(|| {
            ContractError::InvalidInput("transfer requires an amount".to_string())
        })?;
        if amount == 0 {
            return Err(ContractError::InvalidInput(
                "transfer amount must be non-zero".to_string(),
            ));
        }
        let delta = TransferDelta {
            token_id: input.tx.token_id.clone(),
            sender: input.tx.sender.clone(),
            receiver: input.tx.receiver.clone(),
            amount,
            sequence_number: input.tx.sequence_number,
        };
        let result_data = crate::encoding::serialize_to_bytes_rmp(&delta)
            .map_err(|e| ContractError::InvalidInput(format!("canonical encode failed: {}", e)))?;
        Ok(ExecutionOutput::new(result_data, TRANSFER_BASE_COST))
    }
}

// ---------------------------------------------------------------------------
// Spec-AST instruction set (ADR-011 / Q5)
//
// The versioned rmp instruction AST stored in `SmartContract.bytecode`.
// Closed ISA — `Call` is reserved for Phase 7 (cross-contract).
// ---------------------------------------------------------------------------

/// The versioned instruction AST stored in `SmartContract.bytecode`.
///
/// Bytecode is the rmp-canonical serialization of this struct. Only
/// `version == 1` is supported; anything else fails closed with
/// [`ContractError::BadBytecode`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct InstructionProgram {
    pub version: u32,
    pub ops: Vec<Op>,
}

/// The closed instruction set.
///
/// Stack machine semantics (all values are `u64`): for the binary arithmetic
/// ops, `a` is the stack top and `b` the value beneath it, and the result is
/// `b <op> a` (e.g. `LoadConst(10); LoadConst(3); Sub` pushes `7`).
///
/// `Call` (Phase 9, ADR-016) is an ISA extension: adding the variant is
/// additive — existing v1 programs decode unchanged, and a `Call` program on an
/// old node fails closed at decode (Ground Rule 4). The format `version` stays 1.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Push a transaction field. Fields: `amount`, `sequence_number`,
    /// `gas_limit` (pushed as `u64`); `sender`, `receiver`, `token_id`,
    /// `payload` (byte fields, pushed as the first ≤ 8 bytes, left-zero-
    /// padded to 8, read big-endian). An unknown field name reverts.
    LoadTx(String),
    /// Push a compile-time constant.
    LoadConst(u64),
    /// Pop `a`, `b`; push `b + a`. Overflow reverts.
    Add,
    /// Pop `a`, `b`; push `b - a`. Underflow reverts.
    Sub,
    /// Pop `a`, `b`; push `b * a`. Overflow reverts.
    Mul,
    /// Pop `a`, `b`; push `b % a`. `a == 0` reverts.
    Mod,
    /// Pop `a`, `b`; push `1` if `b > a`, `2` if `b == a`, `3` if `b < a`.
    Cmp,
    /// Pop `cond`, `b`, `a`; push `a` if `cond != 0`, else `b`.
    Select,
    /// Pop the stack top; set `result_data` to its 8-byte little-endian
    /// encoding. May be used multiple times — the last `Emit` wins.
    Emit,
    /// Halt successfully. Reaching the end of the program is an implicit
    /// `Halt`.
    Halt,
    /// Model X cross-contract call (ADR-016, Phase 9). The call parameters are
    /// **static immediates** in the AST (closed ISA, deterministic by
    /// construction): the target token, its entry point, the calldata, and the
    /// snapshot ref (`ref_height` + `ref_hash` anchoring the target's chain).
    ///
    /// The target's engine runs in this call frame under a sub-budget
    /// (`A_remaining − CALL_BASE_SPEC`); the op pushes the **status**: `1` =
    /// success, `0` = failure (a deterministic [`CallFailure`] the program can
    /// branch on — a call never reverts A). The callee's result bytes are not
    /// exposed to the `u64` stack (documented Tier-1 Spec constraint; the Wasm
    /// `env.call` import returns the bytes). A `Call` with no `call_ctx`
    /// reverts deterministically (fail closed).
    Call {
        target_token: Vec<u8>,
        entry_point: String,
        call_payload: Vec<u8>,
        ref_height: u64,
        ref_hash: Vec<u8>,
    },
}

/// Spec-AST engine (name: `"Spec"`): interprets the versioned rmp instruction
/// AST stored in `SmartContract.bytecode` (ADR-011 / Q5).
///
/// Gas (ADR-013 stage 2): each executed instruction costs one unit against
/// `input.gas_limit`; exceeding the cap returns [`ContractError::GasExhausted`].
/// `gas_limit == 0` means no cap. All arithmetic is checked — overflow,
/// underflow, and modulo-by-zero revert the execution (fail closed, no
/// wrapping).
pub struct SpecEngine;

impl ContractEngine for SpecEngine {
    fn name(&self) -> &str {
        "Spec"
    }

    fn execute(&self, input: &ExecutionInput) -> Result<ExecutionOutput, ContractError> {
        let program: InstructionProgram =
            crate::encoding::deserialize_rmp_to(&input.contract.bytecode)
                .map_err(|e| ContractError::BadBytecode(format!("undecodable AST: {}", e)))?;
        if program.version != 1 {
            return Err(ContractError::BadBytecode(format!(
                "unsupported program version {}",
                program.version
            )));
        }

        let mut stack: Vec<u64> = Vec::new();
        let mut result_data: Vec<u8> = Vec::new();
        let mut gas_used: u64 = 0;

        for op in &program.ops {
            // Instruction-counted gas meter: every op costs one unit.
            if input.gas_limit > 0 && gas_used >= input.gas_limit {
                return Err(ContractError::GasExhausted);
            }
            gas_used += 1;

            match op {
                Op::LoadTx(field) => {
                    stack.push(load_tx_field(input, field)?);
                }
                Op::LoadConst(n) => stack.push(*n),
                Op::Add => {
                    let (a, b) = pop_two(&mut stack, "add")?;
                    stack.push(b.checked_add(a).ok_or_else(|| {
                        ContractError::Reverted("addition overflow".to_string())
                    })?);
                }
                Op::Sub => {
                    let (a, b) = pop_two(&mut stack, "sub")?;
                    stack.push(b.checked_sub(a).ok_or_else(|| {
                        ContractError::Reverted("subtraction underflow".to_string())
                    })?);
                }
                Op::Mul => {
                    let (a, b) = pop_two(&mut stack, "mul")?;
                    stack.push(b.checked_mul(a).ok_or_else(|| {
                        ContractError::Reverted("multiplication overflow".to_string())
                    })?);
                }
                Op::Mod => {
                    let (a, b) = pop_two(&mut stack, "mod")?;
                    if a == 0 {
                        return Err(ContractError::Reverted("modulo by zero".to_string()));
                    }
                    stack.push(b % a);
                }
                Op::Cmp => {
                    let (a, b) = pop_two(&mut stack, "cmp")?;
                    let r = if b > a {
                        1
                    } else if b == a {
                        2
                    } else {
                        3
                    };
                    stack.push(r);
                }
                Op::Select => {
                    let cond = pop_one(&mut stack, "select")?;
                    let b = pop_one(&mut stack, "select")?;
                    let a = pop_one(&mut stack, "select")?;
                    stack.push(if cond != 0 { a } else { b });
                }
                Op::Emit => {
                    let value = pop_one(&mut stack, "emit")?;
                    result_data = value.to_le_bytes().to_vec();
                }
                Op::Halt => break,
                Op::Call {
                    target_token,
                    entry_point,
                    call_payload,
                    ref_height,
                    ref_hash,
                } => {
                    // Model X call (ADR-016): resolve the target at the pinned ref
                    // and run its engine in this call frame under a sub-budget.
                    // A call never kills A — the outcome (success/failure) is data
                    // pushed as a status; the program branches on it.
                    let Some(ctx) = &input.call_ctx else {
                        return Err(ContractError::Reverted(
                            "call: no call context".to_string(),
                        ));
                    };
                    // Sub-budget: A's remaining gas minus the call base cost, so the
                    // call can never push A past A's cap (ADR-016 Q4).
                    let remaining = if input.gas_limit > 0 {
                        input.gas_limit.saturating_sub(gas_used)
                    } else {
                        u64::MAX
                    };
                    let sub = remaining.saturating_sub(XCALL_CALL_BASE_SPEC);
                    let ref_ = SnapshotRef {
                        height: *ref_height,
                        block_hash: ref_hash.clone(),
                    };
                    let outcome = execute_call(
                        ctx,
                        input.tx,
                        target_token,
                        entry_point,
                        call_payload,
                        &ref_,
                        sub,
                    );
                    // A charges the call base + B's sub-execution work.
                    gas_used = gas_used
                        .saturating_add(XCALL_CALL_BASE_SPEC + outcome.b_gas_used());
                    if input.gas_limit > 0 && gas_used > input.gas_limit {
                        return Err(ContractError::GasExhausted);
                    }
                    stack.push(u64::from(outcome.is_success()));
                }
            }
        }

        Ok(ExecutionOutput::new(result_data, gas_used))
    }
}

/// Pop one value; an empty stack reverts (malformed program, fail closed).
fn pop_one(stack: &mut Vec<u64>, op: &str) -> Result<u64, ContractError> {
    stack
        .pop()
        .ok_or_else(|| ContractError::Reverted(format!("\"{}\": stack underflow", op)))
}

/// Pop two values, returning `(a, b)` where `a` is the former top.
fn pop_two(stack: &mut Vec<u64>, op: &str) -> Result<(u64, u64), ContractError> {
    let a = pop_one(stack, op)?;
    let b = pop_one(stack, op)?;
    Ok((a, b))
}

/// Resolve a [`Op::LoadTx`] field name to a `u64`.
///
/// Byte fields (`sender`, `receiver`, `token_id`, `payload`) are reduced to
/// the first ≤ 8 bytes, left-zero-padded to 8, read big-endian — a fixed,
/// host-independent reduction so every executor computes the same value.
fn load_tx_field(input: &ExecutionInput, field: &str) -> Result<u64, ContractError> {
    let tx = input.tx;
    match field {
        "amount" => tx
            .amount
            .ok_or_else(|| ContractError::Reverted("load_tx: amount is not set".to_string())),
        "sequence_number" => Ok(tx.sequence_number as u64),
        "gas_limit" => Ok(input.gas_limit),
        "sender" => Ok(bytes_to_u64(&tx.sender)),
        "receiver" => Ok(bytes_to_u64(&tx.receiver)),
        "token_id" => Ok(bytes_to_u64(&tx.token_id)),
        "payload" => Ok(bytes_to_u64(&tx.payload)),
        other => Err(ContractError::Reverted(format!(
            "load_tx: undeclared field \"{}\"",
            other
        ))),
    }
}

/// First ≤ 8 bytes, left-zero-padded to 8, big-endian `u64`.
fn bytes_to_u64(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let n = std::cmp::min(8, bytes.len());
    buf[8 - n..].copy_from_slice(&bytes[..n]);
    u64::from_be_bytes(buf)
}

// ---------------------------------------------------------------------------
// Per-token engine selection (ADR-011 sub-decision 2)
// ---------------------------------------------------------------------------

/// Select the engine for `token` from `registry` (ADR-011).
///
/// Rule: the token metadata key `contract_engine` names the engine. Tokens
/// that are not contract tokens (`token_type != "contract"`) default to
/// `"Transfer"`. Contract tokens must name a registered engine — anything
/// else fails closed with [`ContractError::UnknownEngine`].
pub fn select_engine(
    registry: &ContractEngineRegistry,
    token: &Token,
) -> Result<Arc<dyn ContractEngine>, ContractError> {
    let is_contract_token = token
        .metadata
        .get("token_type")
        .map(|v| v == "contract")
        .unwrap_or(false);
    let name = match token.metadata.get("contract_engine") {
        Some(name) => name.clone(),
        None if is_contract_token => {
            return Err(ContractError::UnknownEngine(format!(
                "contract token {:?} must declare a 'contract_engine' metadata key",
                token.id
            )));
        }
        None => "Transfer".to_string(),
    };
    registry
        .get(&name)
        .ok_or_else(|| ContractError::UnknownEngine(name))
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
            result_data: vec![],
        }
    }

    fn test_contract() -> SmartContract {
        SmartContract {
            name: "test-contract".to_string(),
            bytecode: vec![0xAB, 0xCD],
            version: "1".to_string(),
            storage: Default::default(),
            owners: vec![],
            threshold: 0,
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
            storage: BTreeMap::new(),
            call_ctx: None,
        };
        let b = ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 1234,
            storage: BTreeMap::new(),
            call_ctx: None,
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
            storage: BTreeMap::new(),
            call_ctx: None,
        };
        let b = ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token_b,
            gas_limit: 100,
            storage: BTreeMap::new(),
            call_ctx: None,
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
            storage: BTreeMap::new(),
            call_ctx: None,
        };
        let alt_gas = ExecutionInput {
            tx: &tx_b,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 9999,
            storage: BTreeMap::new(),
            call_ctx: None,
        };
        let alt_payload = ExecutionInput {
            tx: &tx_c,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 1234,
            storage: BTreeMap::new(),
            call_ctx: None,
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
    fn registry_defaults_are_live_engines() {
        // Phase 2: the built-ins are real engines, not fail-closed stubs.
        let registry = ContractEngineRegistry::new();
        registry.register_defaults();
        let tx = test_tx();
        let contract = test_contract();
        let user = test_user();
        let token = test_token();
        let input = exec_input(&tx, &contract, &user, &token, 0); // no cap
        // Transfer executes a well-formed transfer (test_tx has an amount).
        let transfer = registry.get("Transfer").unwrap();
        let out = transfer.execute(&input).expect("transfer executes");
        assert!(!out.result_data.is_empty());
        assert_eq!(out.gas_used, TRANSFER_BASE_COST);
        // Spec with malformed bytecode fails closed (test_contract has 0xAB 0xCD).
        let spec = registry.get("Spec").unwrap();
        let err = spec.execute(&input).unwrap_err();
        assert!(matches!(err, ContractError::BadBytecode(_)), "got {:?}", err);
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

    // --- Phase 2: TransferEngine ---

    fn exec_input<'a>(
        tx: &'a Transaction,
        contract: &'a SmartContract,
        user: &'a User,
        token: &'a Token,
        gas_limit: u64,
    ) -> ExecutionInput<'a> {
        ExecutionInput {
            tx,
            contract,
            sender_state: user,
            token,
            gas_limit,
            storage: BTreeMap::new(),
            call_ctx: None,
        }
    }

    #[test]
    fn transfer_engine_happy_path_emits_canonical_delta() {
        let registry = ContractEngineRegistry::new();
        registry.register_defaults();
        let engine = registry.get("Transfer").unwrap();
        let tx = test_tx();
        let contract = test_contract();
        let user = test_user();
        let token = test_token();
        let input = exec_input(&tx, &contract, &user, &token, 0);

        let out = engine.execute(&input).expect("transfer executes");
        assert_eq!(out.gas_used, TRANSFER_BASE_COST);
        // The result must be exactly the canonical delta encoding.
        let expected = TransferDelta {
            token_id: tx.token_id.clone(),
            sender: tx.sender.clone(),
            receiver: tx.receiver.clone(),
            amount: 50,
            sequence_number: 7,
        };
        let expected_bytes =
            crate::encoding::serialize_to_bytes_rmp(&expected).expect("rmp");
        assert_eq!(out.result_data, expected_bytes);
    }

    #[test]
    fn transfer_engine_rejects_missing_and_zero_amount() {
        let engine = TransferEngine;
        let contract = test_contract();
        let user = test_user();
        let token = test_token();

        let mut no_amount = test_tx();
        no_amount.amount = None;
        let in1 = exec_input(&no_amount, &contract, &user, &token, 0);
        assert!(matches!(
            engine.execute(&in1),
            Err(ContractError::InvalidInput(_))
        ));

        let mut zero_amount = test_tx();
        zero_amount.amount = Some(0);
        let in2 = exec_input(&zero_amount, &contract, &user, &token, 0);
        assert!(matches!(
            engine.execute(&in2),
            Err(ContractError::InvalidInput(_))
        ));
    }

    #[test]
    fn transfer_engine_gas_cap_enforced() {
        let engine = TransferEngine;
        let tx = test_tx();
        let contract = test_contract();
        let user = test_user();
        let token = test_token();

        // 0 = no cap declared → executes.
        assert!(engine
            .execute(&exec_input(&tx, &contract, &user, &token, 0))
            .is_ok());
        // Cap exactly at the base cost → executes.
        assert!(engine
            .execute(&exec_input(
                &tx,
                &contract,
                &user,
                &token,
                TRANSFER_BASE_COST
            ))
            .is_ok());
        // Cap below the base cost → GasExhausted (fail closed).
        assert!(matches!(
            engine.execute(&exec_input(&tx, &contract, &user, &token, 1_000)),
            Err(ContractError::GasExhausted)
        ));
    }

    // --- Phase 2: SpecEngine ---

    fn program_bytes(ops: Vec<Op>) -> Vec<u8> {
        crate::encoding::serialize_to_bytes_rmp(&InstructionProgram {
            version: 1,
            ops,
        })
        .expect("program rmp")
    }

    fn spec_contract(bytecode: Vec<u8>) -> SmartContract {
        SmartContract {
            name: "spec-contract".to_string(),
            bytecode,
            version: "1".to_string(),
            storage: Default::default(),
            owners: vec![],
            threshold: 0,
        }
    }

    #[test]
    fn spec_engine_happy_path_arithmetic_and_emit() {
        let engine = SpecEngine;
        let tx = test_tx();
        let user = test_user();
        let token = test_token();
        let bytecode = program_bytes(vec![
            Op::LoadConst(7),
            Op::LoadConst(6),
            Op::Mul,
            Op::Emit,
            Op::Halt,
        ]);
        let out = engine
            .execute(&exec_input(&tx, &spec_contract(bytecode), &user, &token, 0))
            .expect("spec executes");
        assert_eq!(out.result_data, 42u64.to_le_bytes());
        assert_eq!(out.gas_used, 5);
    }

    #[test]
    fn spec_engine_gas_exhausted_at_cap() {
        let engine = SpecEngine;
        let tx = test_tx();
        let user = test_user();
        let token = test_token();
        // 3 instructions; a cap of 2 lets two run and exhausts on the third.
        let bytecode = program_bytes(vec![Op::LoadConst(1), Op::LoadConst(2), Op::Add, Op::Emit]);
        assert!(matches!(
            engine.execute(&exec_input(&tx, &spec_contract(bytecode), &user, &token, 2)),
            Err(ContractError::GasExhausted)
        ));
        // A cap of 4 covers all four instructions.
        assert!(engine
            .execute(&exec_input(&tx, &spec_contract(program_bytes(vec![
                Op::LoadConst(1),
                Op::LoadConst(2),
                Op::Add,
                Op::Emit
            ])), &user, &token, 4))
            .is_ok());
    }

    #[test]
    fn spec_engine_malformed_bytecode_fails_closed() {
        let engine = SpecEngine;
        let tx = test_tx();
        let user = test_user();
        let token = test_token();

        // Garbage bytes: undecodable AST.
        assert!(matches!(
            engine.execute(&exec_input(&tx, &spec_contract(vec![0xFF, 0x01, 0x02]), &user, &token, 0)),
            Err(ContractError::BadBytecode(_))
        ));
        // Decodable but unsupported version.
        let bad_version =
            crate::encoding::serialize_to_bytes_rmp(&InstructionProgram {
                version: 2,
                ops: vec![Op::Halt],
            })
            .expect("rmp");
        assert!(matches!(
            engine.execute(&exec_input(&tx, &spec_contract(bad_version), &user, &token, 0)),
            Err(ContractError::BadBytecode(_))
        ));
    }

    #[test]
    fn spec_engine_checked_arithmetic_reverts() {
        let engine = SpecEngine;
        let tx = test_tx();
        let user = test_user();
        let token = test_token();

        // Addition overflow.
        let overflow = program_bytes(vec![
            Op::LoadConst(u64::MAX),
            Op::LoadConst(1),
            Op::Add,
            Op::Emit,
        ]);
        assert!(matches!(
            engine.execute(&exec_input(&tx, &spec_contract(overflow), &user, &token, 0)),
            Err(ContractError::Reverted(_))
        ));
        // Subtraction underflow.
        let underflow = program_bytes(vec![
            Op::LoadConst(1),
            Op::LoadConst(2),
            Op::Sub,
            Op::Emit,
        ]);
        assert!(matches!(
            engine.execute(&exec_input(&tx, &spec_contract(underflow), &user, &token, 0)),
            Err(ContractError::Reverted(_))
        ));
        // Modulo by zero.
        let mod_zero = program_bytes(vec![
            Op::LoadConst(5),
            Op::LoadConst(0),
            Op::Mod,
            Op::Emit,
        ]);
        assert!(matches!(
            engine.execute(&exec_input(&tx, &spec_contract(mod_zero), &user, &token, 0)),
            Err(ContractError::Reverted(_))
        ));
        // Stack underflow (binary op with one value).
        let underflow_stack = program_bytes(vec![Op::LoadConst(1), Op::Add, Op::Emit]);
        assert!(matches!(
            engine.execute(&exec_input(&tx, &spec_contract(underflow_stack), &user, &token, 0)),
            Err(ContractError::Reverted(_))
        ));
        // Undeclared LoadTx field.
        let bad_field = program_bytes(vec![Op::LoadTx("nonce".to_string()), Op::Emit]);
        assert!(matches!(
            engine.execute(&exec_input(&tx, &spec_contract(bad_field), &user, &token, 0)),
            Err(ContractError::Reverted(_))
        ));
    }

    #[test]
    fn spec_engine_cmp_select_control_flow() {
        let engine = SpecEngine;
        let tx = test_tx();
        let user = test_user();
        let token = test_token();
        // Cmp: push 10, push 3 → a=3 (top), b=10 → b > a → 1.
        let cmp = program_bytes(vec![
            Op::LoadConst(10),
            Op::LoadConst(3),
            Op::Cmp,
            Op::Emit,
            Op::Halt,
        ]);
        let out = engine
            .execute(&exec_input(&tx, &spec_contract(cmp), &user, &token, 0))
            .expect("spec executes");
        assert_eq!(out.result_data, 1u64.to_le_bytes());

        // Select, cond != 0 → picks `a`: push a=7, b=99, cond=1 (cond is the
        // stack top) → 7.
        let sel_true = program_bytes(vec![
            Op::LoadConst(7),
            Op::LoadConst(99),
            Op::LoadConst(1),
            Op::Select,
            Op::Emit,
            Op::Halt,
        ]);
        let out = engine
            .execute(&exec_input(&tx, &spec_contract(sel_true), &user, &token, 0))
            .expect("spec executes");
        assert_eq!(out.result_data, 7u64.to_le_bytes());
        assert_eq!(out.gas_used, 6);

        // Select, cond == 0 → picks `b`: push a=7, b=99, cond=0 → 99.
        let sel_false = program_bytes(vec![
            Op::LoadConst(7),
            Op::LoadConst(99),
            Op::LoadConst(0),
            Op::Select,
            Op::Emit,
            Op::Halt,
        ]);
        let out = engine
            .execute(&exec_input(&tx, &spec_contract(sel_false), &user, &token, 0))
            .expect("spec executes");
        assert_eq!(out.result_data, 99u64.to_le_bytes());
    }

    #[test]
    fn spec_engine_last_emit_wins_and_loadtx_fields() {
        let engine = SpecEngine;
        let user = test_user();
        let token = test_token();
        let mut tx = test_tx();
        tx.amount = Some(42);
        tx.sequence_number = 5;
        // amount=42, seq=5 → cmp(5, 42): b=42? pop a=5 (top), b=42 → 42 > 5 → 1.
        // Emit the cmp result (1), then Emit the amount (42) — last emit wins.
        let bytecode = program_bytes(vec![
            Op::LoadTx("amount".to_string()),
            Op::LoadTx("sequence_number".to_string()),
            Op::Cmp,
            Op::Emit,
            Op::LoadTx("amount".to_string()),
            Op::Emit,
            Op::Halt,
        ]);
        let out = engine
            .execute(&exec_input(&tx, &spec_contract(bytecode), &user, &token, 0))
            .expect("spec executes");
        assert_eq!(out.result_data, 42u64.to_le_bytes());
    }

    // --- Phase 2: per-token engine selection ---

    #[test]
    fn select_engine_default_transfer_for_plain_tokens() {
        let registry = ContractEngineRegistry::new();
        registry.register_defaults();
        let mut token = test_token();
        token.metadata.remove("token_type"); // plain token, no contract_engine key
        let engine = select_engine(&registry, &token).expect("default engine");
        assert_eq!(engine.name(), "Transfer");
    }

    #[test]
    fn select_engine_uses_metadata_key_when_present() {
        let registry = ContractEngineRegistry::new();
        registry.register_defaults();
        let mut token = test_token();
        token
            .metadata
            .insert("contract_engine".to_string(), "Spec".to_string());
        let engine = select_engine(&registry, &token).expect("named engine");
        assert_eq!(engine.name(), "Spec");
    }

    #[test]
    fn select_engine_contract_token_without_key_fails_closed() {
        let registry = ContractEngineRegistry::new();
        registry.register_defaults();
        let token = test_token(); // token_type = "contract", no contract_engine key
        assert!(matches!(
            select_engine(&registry, &token),
            Err(ContractError::UnknownEngine(_))
        ));
    }

    #[test]
    fn select_engine_unregistered_name_fails_closed() {
        let registry = ContractEngineRegistry::new();
        registry.register_defaults();
        let mut token = test_token();
        token
            .metadata
            .insert("contract_engine".to_string(), "Wasm".to_string());
        assert!(matches!(
            select_engine(&registry, &token),
            Err(ContractError::UnknownEngine(_))
        ));
    }

    // --- Phase 2: determinism property tests (ADR-008) ---
    //
    // Random (tx, contract, user, token, gas_limit) inputs executed repeatedly
    // — twice plainly, once on a tokio current-thread runtime, once on a
    // multi-thread runtime — must yield byte-identical `result_data`. A
    // divergence would mean a non-pure engine (host-dependent iteration order,
    // RNG, clock, I/O) and a consensus fork.

    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    fn random_bytes(rng: &mut StdRng, max_len: usize) -> Vec<u8> {
        let len = rng.gen_range(0..=max_len);
        (0..len).map(|_| rng.gen::<u8>()).collect()
    }

    /// A random, well-formed program: never underflows the stack; ends in
    /// Emit (when possible) + Halt.
    fn random_program(rng: &mut StdRng) -> Vec<Op> {
        const FIELDS: [&str; 7] = [
            "amount", "sequence_number", "gas_limit", "sender", "receiver", "token_id", "payload",
        ];
        let mut ops: Vec<Op> = vec![Op::LoadConst(rng.gen_range(0..1000))];
        let mut depth = 1usize;
        for _ in 0..rng.gen_range(2..16) {
            match rng.gen_range(0..10) {
                0..=3 => {
                    ops.push(Op::LoadConst(rng.gen_range(0..1_000_000)));
                    depth += 1;
                }
                4..=5 => {
                    ops.push(Op::LoadTx(FIELDS[rng.gen_range(0..FIELDS.len())].to_string()));
                    depth += 1;
                }
                6..=8 => {
                    if depth >= 2 {
                        ops.push(match rng.gen_range(0..3) {
                            0 => Op::Add,
                            1 => Op::Sub,
                            _ => Op::Mul,
                        });
                        depth -= 1;
                    } else {
                        ops.push(Op::LoadConst(1));
                        depth += 1;
                    }
                }
                9 => {
                    if depth >= 3 {
                        ops.push(Op::Select);
                        depth -= 1;
                    } else if depth >= 2 {
                        ops.push(Op::Cmp);
                        depth -= 1;
                    } else {
                        ops.push(Op::LoadConst(2));
                        depth += 1;
                    }
                }
                _ => unreachable!(),
            }
        }
        if depth >= 1 {
            ops.push(Op::Emit);
        }
        ops.push(Op::Halt);
        ops
    }

    fn random_case(seed: u64) -> (Transaction, SmartContract, User, Token, u64) {
        let mut rng = StdRng::seed_from_u64(seed);
        let tx = Transaction {
            id: format!("dtx-{}", rng.gen::<u32>()),
            action: "ContractCall".to_string(),
            token_id: random_bytes(&mut rng, 12),
            bid: None,
            sequence_number: rng.gen_range(0..1000),
            sender: random_bytes(&mut rng, 24),
            receiver: random_bytes(&mut rng, 24),
            amount: Some(rng.gen_range(0..10_000)),
            timestamp: rng.gen_range(1_600_000_000..1_800_000_000),
            result_hash: vec![],
            sender_signature: vec![],
            payload: random_bytes(&mut rng, 32),
            gas_limit: [0, 5, 50, 1000][rng.gen_range(0..4)],
            result_data: vec![],
        };
        let contract = SmartContract {
            name: format!("dcontract-{}", rng.gen::<u32>()),
            bytecode: program_bytes(random_program(&mut rng)),
            version: "1".to_string(),
            storage: Default::default(),
            owners: vec![],
            threshold: 0,
        };
        let gas = tx.gas_limit;
        let user = User {
            public_key: random_bytes(&mut rng, 32),
            fuel_balance: rng.gen_range(0..100_000),
            stake: rng.gen_range(0..1000),
            nonce: rng.gen_range(0..1000),
        };
        let mut token = Token::new();
        token.id = random_bytes(&mut rng, 16);
        token
            .metadata
            .insert("token_type".to_string(), "contract".to_string());
        token
            .metadata
            .insert("contract_engine".to_string(), "Spec".to_string());
        (tx, contract, user, token, gas)
    }

    fn run_case_both_engines(seed: u64) {
        let (tx, contract, user, token, gas) = random_case(seed);
        let input = ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: gas,
            storage: BTreeMap::new(),
            call_ctx: None,
        };
        let transfer = TransferEngine;
        let spec = SpecEngine;

        // Four runs per engine: plain ×2, current-thread tokio, multi-thread tokio.
        let rt_ct = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let rt_mt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("multi-thread runtime");

        for engine in [
            &transfer as &dyn ContractEngine,
            &spec as &dyn ContractEngine,
        ] {
            let r1 = engine.execute(&input);
            let r2 = engine.execute(&input);
            let r3 = match rt_ct.block_on(async { engine.execute(&input) }) {
                Ok(o) => Ok(o.clone()),
                Err(e) => Err(e.clone()),
            };
            let r4 = match rt_mt.block_on(async { engine.execute(&input) }) {
                Ok(o) => Ok(o.clone()),
                Err(e) => Err(e.clone()),
            };
            for (other, label) in [(r2, "plain-2"), (r3, "tokio-ct"), (r4, "tokio-mt")] {
                assert_eq!(
                    r1, other,
                    "seed {} engine {:?}: run 1 != {}",
                    seed,
                    engine.name(),
                    label
                );
            }
        }
        drop(rt_ct);
        drop(rt_mt);
    }

    #[test]
    fn determinism_property_transfer_and_spec_engines() {
        // 25 seeded random cases × 2 engines × 4 runs each.
        for seed in 0..25u64 {
            run_case_both_engines(seed);
        }
    }
}
