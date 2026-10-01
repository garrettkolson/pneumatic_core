//! WasmEngine — the Tier-2 pluggable contract engine (ADR-011 / ADR-018).
//!
//! Runs third-party contract `bytecode` in a deterministic WebAssembly sandbox
//! (`wasmi`, a pure-Rust interpreter — no JIT, so results are host-independent, which
//! is consensus-critical per ADR-008).
//!
//! ## Gas model (ADR-013)
//! WASM **fuel is the gas**. The module's fuel budget is `input.gas_limit`
//! (`0` ⇒ effectively unbounded). `wasmi` consumes fuel per instruction; running out
//! of fuel traps, which we map to [`ContractError::GasExhausted`] (→ the executor's
//! `Failed` transition, no vote). `gas_used` is the fuel actually consumed
//! (`budget − remaining`), consistent with the other engines' "work done" accounting.
//!
//! ## Frozen ABI (ADR-018 §6)
//! A module must export:
//! - `__alloc(size: i32) -> i32` — allocate `size` bytes in linear memory, return ptr.
//! - `execute(input_ptr, input_len, output_ptr, output_cap: i32) -> i32` — read the
//!   canonical [`ExecutionInput`] bytes at `[input_ptr, input_ptr+input_len)`, compute,
//!   write the `result_data` (rmp-canonical) at `[output_ptr, …)` and return its length
//!   (must be `<= output_cap`).
//! The host allocates both buffers via `__alloc`, so the host fully controls memory.
//!
//! ## Sandbox (ADR-018 §7)
//! Only the `env` import namespace is allowed, and only the allow-listed read-only
//! host functions (capability model, W2 tier). No net/FS/env/clock/RNG exist in the
//! `wasmi` interpreter by default. `f32`/`f64` are disallowed on the host-facing ABI
//! boundary (integer-only); internal floating point is deterministic under a pinned
//! `wasmi` (pure-Rust IEEE-754), so it is permitted.
//!
//! ## Caps (ADR-018 §8, protocol-tunable)
//! Module size ≤ 1 MiB; linear memory ≤ 32 MiB (512 pages). Fuel metering is the
//! primary bound on memory growth (each `memory.grow` costs fuel); a post-call check
//! enforces the 32 MiB ceiling.

use std::collections::BTreeMap;
use std::sync::Arc;

use wasmi::{
    Caller, Config, Engine, Extern, ExternType, Instance, Linker, Memory, Module, Store,
    TrapCode, ValType,
};

use crate::encoding::serialize_to_bytes_rmp;
use crate::transactions::Transaction;
use super::call::{execute_call, SnapshotRef, XCALL_CALL_BASE_WASM};
use super::{CallContext, ContractEngine, ContractError, ExecutionInput, ExecutionOutput, WasmResult};

// ---------------------------------------------------------------------------
// Caps (ADR-018 QW6) — protocol-tunable.
// ---------------------------------------------------------------------------

/// Maximum compiled-module size in bytes (1 MiB).
pub const WASM_MAX_MODULE_BYTES: usize = 1 * 1024 * 1024;
/// Maximum linear memory in bytes (512 pages = 32 MiB).
pub const WASM_MAX_MEMORY_BYTES: usize = 512 * 64 * 1024;
/// Output buffer the host allocates for a module's `result_data` (16 KiB).
///
/// Sized conservatively: the buffer is carved from the module's own linear-memory
/// heap (via the module's `__alloc`), which can be small. A module compiled with a
/// minimal memory (e.g. the 17-page / 1088 KiB test fixtures, whose `__heap_base`
/// sits at 1 MiB) has only ~64 KiB of heap; the host's input + output buffers must
/// leave room for the module's own allocations (storage keys/values, etc.). 16 KiB
/// is far above any realistic `result_data` size while staying safely in budget.
const WASM_OUTPUT_CAP: u32 = 16 * 1024;
/// Fuel budget used when `gas_limit == 0` (effectively unbounded).
const UNLIMITED_FUEL: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// W3 storage gas + cap (ADR-018 / Phase 7, QD-gas-cap).
// ---------------------------------------------------------------------------

/// Gas charged per `sload` (a read-your-writes lookup).
pub const WASM_SLOAD_GAS: u64 = 100;
/// Gas charged per `sstore` of a **new** key (first write to an absent key).
pub const WASM_SSTORE_NEW_GAS: u64 = 20_000;
/// Gas charged per `sstore` that **rewrites** an existing key.
pub const WASM_SSTORE_REWRITE_GAS: u64 = 5_000;
/// Gas charged per `sdelete` (tombstone).
pub const WASM_SDELETE_GAS: u64 = 5_000;
/// Per-contract state cap in bytes (sum of key + value lengths post-apply).
pub const WASM_STORAGE_CAP: usize = 1 * 1024 * 1024;

/// Allow-listed `env.*` host imports (the W2 read-only capability tier + revert,
/// plus the W3 state tier: `sload`/`sstore`/`sdelete`).
/// Any other import (or any other namespace) is rejected before instantiation.
///
/// `pub(crate)` so the deploy-time contract scanner (`scan.rs`) reuses the *exact*
/// same list in its static walk — a single source of truth, so the scanner can never
/// drift from the runtime sandbox.
pub(crate) const ALLOWED_ENV_IMPORTS: &[&str] = &[
    "tx_amount",
    "tx_sequence",
    "sender_fuel",
    "tx_payload_len",
    "tx_payload",
    "revert",
    // W3 state tier (ADR-018 / Phase 7).
    "sload",
    "sstore",
    "sdelete",
    // Model X cross-contract call (ADR-016 / Phase 9).
    "call",
];

// ---------------------------------------------------------------------------
// Host state + revert marker
// ---------------------------------------------------------------------------

/// The execution-time-snapshot state the `env.*` host functions read (and the W3
/// state tier mutates). Owned by the [`Store`] so the `'static` host closures
/// (required by [`Linker::func_wrap`]) can reach it via `caller.data()` /
/// `caller.data_mut()`. Every read-only field is a pure function of
/// [`ExecutionInput`] — identical across shard members (Model-X invariant). The
/// storage fields are per-call scratch: `storage` is the base state (read),
/// `storage_delta` is the write-set the module records (read-your-writes + the
/// canonical output delta), `storage_gas` accumulates the W3 storage cost.
struct WasmEnv {
    /// The calling (A) transaction — the basis for the virtual-tx synthesis in
    /// `env.call` (ADR-016).
    tx: Transaction,
    tx_amount: i64,
    tx_sequence: i64,
    sender_fuel: i64,
    payload: Vec<u8>,
    /// Base contract state for this call (from [`ExecutionInput::storage`]).
    /// `sload` reads here after checking [`WasmEnv::storage_delta`].
    storage: BTreeMap<Vec<u8>, Vec<u8>>,
    /// The module's write-set this call (read-your-writes + the output delta).
    /// `Some(v)` = set; `None` = tombstone. Drains into the [`WasmResult`] envelope.
    storage_delta: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// Accumulated W3 storage gas (`sload`/`sstore`/`sdelete` costs).
    storage_gas: u64,
    /// The Model X call context (ADR-016, Phase 9): `None` = no call capability
    /// (an `env.call` returns `0` deterministically, never traps).
    call_ctx: Option<Arc<CallContext>>,
    /// Accumulated Model X call gas (`XCALL_CALL_BASE_WASM + B work` per `env.call`).
    call_gas: u64,
}

/// Marker [`wasmi::errors::HostError`] the `env.revert()` import traps with. The
/// engine detects it via `downcast_ref` and maps it to [`ContractError::Reverted`].
#[derive(Debug)]
struct RevertSignal(String);

impl std::fmt::Display for RevertSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "contract reverted: {}", self.0)
    }
}

impl wasmi::errors::HostError for RevertSignal {}

/// Read up to `len` bytes from the module's linear memory at `ptr` (clamped to the
/// memory bounds). Returns an owned `Vec` so the `caller` borrow ends before the
/// next (possibly mutable) host op. Used by the W3 state-tier imports.
fn read_mem(caller: &mut Caller<WasmEnv>, ptr: i32, len: i32) -> Result<Vec<u8>, wasmi::Error> {
    let mem = caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmi::Error::new("no linear memory"))?;
    let data = mem.data(caller);
    let start = ptr as usize;
    let n = (len as usize).min(data.len().saturating_sub(start));
    Ok(data[start..start + n].to_vec())
}

/// Write `bytes` to the module's linear memory at `ptr` (clamped to the memory
/// bounds). Used by the W3 state-tier imports to return `sload` values.
fn write_mem(caller: &mut Caller<WasmEnv>, ptr: i32, bytes: &[u8]) -> Result<(), wasmi::Error> {
    let mem = caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmi::Error::new("no linear memory"))?;
    let mut data = mem.data_mut(caller);
    let start = ptr as usize;
    let n = bytes.len().min(data.len().saturating_sub(start));
    data[start..start + n].copy_from_slice(&bytes[..n]);
    Ok(())
}

// ---------------------------------------------------------------------------
// WasmEngine
// ---------------------------------------------------------------------------

/// The Tier-2 [`ContractEngine`]: deterministic WebAssembly execution.
///
/// Stateless (a unit struct), so trivially `Send + Sync`. All per-call state lives in
/// a per-call [`Store`], so no mutable runtime state is shared across executor threads.
pub struct WasmEngine;

impl WasmEngine {
    /// A deterministic `wasmi` engine: fuel metering on (required for `gas_limit`),
    /// the default `wasmi` feature set (no JIT).
    fn build_engine() -> Result<Engine, ContractError> {
        let mut config = Config::default();
        config.consume_fuel(true);
        Ok(Engine::new(&config))
    }

    /// Validate the module's ABI and sandbox before instantiation:
    /// - required exports `__alloc` + `execute`;
    /// - only `env.*` imports, and only the allow-listed ones;
    /// - no `f32`/`f64` on the host-facing ABI (export) boundary.
    fn validate_abi(module: &Module) -> Result<(), ContractError> {
        let mut has_alloc = false;
        let mut has_execute = false;
        for export in module.exports() {
            let name = export.name();
            match name {
                "__alloc" => has_alloc = true,
                "execute" => has_execute = true,
                _ => {}
            }
            if Self::extern_type_has_float(export.ty()) {
                return Err(ContractError::BadBytecode(format!(
                    "export \"{name}\" uses f32/f64 (integer-only ABI)"
                )));
            }
        }
        if !has_alloc {
            return Err(ContractError::BadBytecode("missing __alloc export".into()));
        }
        if !has_execute {
            return Err(ContractError::BadBytecode("missing execute export".into()));
        }
        for import in module.imports() {
            let ns = import.module();
            let name = import.name();
            if ns != "env" {
                return Err(ContractError::BadBytecode(format!(
                    "disallowed import namespace \"{ns}\" (only \"env\" is permitted)"
                )));
            }
            if !ALLOWED_ENV_IMPORTS.contains(&name) {
                return Err(ContractError::BadBytecode(format!(
                    "disallowed import \"env.{name}\""
                )));
            }
        }
        Ok(())
    }

    /// `true` if an ABI-boundary type uses `f32`/`f64`.
    fn extern_type_has_float(t: &ExternType) -> bool {
        match t {
            ExternType::Func(ft) => ft
                .params()
                .iter()
                .chain(ft.results().iter())
                .any(|t| matches!(t, ValType::F32 | ValType::F64)),
            ExternType::Global(gt) => matches!(gt.content(), ValType::F32 | ValType::F64),
            // Memory / Table have no value types.
            ExternType::Memory(_) | ExternType::Table(_) => false,
        }
    }

    /// Link the allow-listed `env.*` host functions (W2 read-only + revert).
    fn link_env(linker: &mut Linker<WasmEnv>) -> Result<(), ContractError> {
        let link_err = |e: wasmi::errors::LinkerError| ContractError::BadBytecode(format!("link: {e}"));
        linker
            .func_wrap::<(Caller<WasmEnv>,), _>("env", "tx_amount", |caller: Caller<WasmEnv>| {
                Ok(caller.data().tx_amount)
            })
            .map_err(link_err)?;
        linker
            .func_wrap::<(Caller<WasmEnv>,), _>("env", "tx_sequence", |caller: Caller<WasmEnv>| {
                Ok(caller.data().tx_sequence)
            })
            .map_err(link_err)?;
        linker
            .func_wrap::<(Caller<WasmEnv>,), _>("env", "sender_fuel", |caller: Caller<WasmEnv>| {
                Ok(caller.data().sender_fuel)
            })
            .map_err(link_err)?;
        linker
            .func_wrap::<(Caller<WasmEnv>,), _>("env", "tx_payload_len", |caller: Caller<WasmEnv>| {
                Ok(caller.data().payload.len() as i32)
            })
            .map_err(link_err)?;
        linker
            .func_wrap::<(Caller<WasmEnv>, i32, i32), _>("env", "tx_payload", |mut caller: Caller<WasmEnv>, ptr: i32, len: i32| {
                // Clone the payload into an owned `Vec` so the immutable `caller`
                // borrow ends before the mutable borrow needed for the memory write.
                let payload: Vec<u8> = caller.data().payload.clone();
                let n = (len as usize).min(payload.len());
                if let Some(mem) = caller
                    .get_export("memory")
                    .and_then(Extern::into_memory)
                {
                    let mut data = mem.data_mut(&mut caller);
                    data[ptr as usize..ptr as usize + n].copy_from_slice(&payload[..n]);
                }
                Ok(())
            })
            .map_err(link_err)?;
        linker
            .func_wrap::<(Caller<WasmEnv>,), _>("env", "revert", |_caller: Caller<WasmEnv>| -> Result<(), wasmi::Error> {
                Err(wasmi::Error::host(RevertSignal(
                    "guest called env.revert()".into(),
                )))
            })
            .map_err(link_err)?;
        // W3 state tier (ADR-018 / Phase 7). `sload` is read-your-writes (delta
        // first, then base); `sstore`/`sdelete` record into `storage_delta` and
        // charge gas. The module passes key/value as linear-memory slices.
        linker
            .func_wrap::<(Caller<WasmEnv>, i32, i32, i32), _>("env", "sload", |mut caller: Caller<WasmEnv>, key_ptr: i32, key_len: i32, out_ptr: i32| {
                let key = read_mem(&mut caller, key_ptr, key_len)?;
                let value: Vec<u8> = {
                    let env = caller.data_mut();
                    env.storage_gas = env.storage_gas.saturating_add(WASM_SLOAD_GAS);
                    match env.storage_delta.get(&key) {
                        Some(Some(v)) => v.clone(),
                        Some(None) => Vec::new(),
                        None => env.storage.get(&key).cloned().unwrap_or_default(),
                    }
                };
                write_mem(&mut caller, out_ptr, &value)?;
                Ok(value.len() as i32)
            })
            .map_err(link_err)?;
        linker
            .func_wrap::<(Caller<WasmEnv>, i32, i32, i32, i32), _>("env", "sstore", |mut caller: Caller<WasmEnv>, key_ptr: i32, key_len: i32, value_ptr: i32, value_len: i32| {
                let key = read_mem(&mut caller, key_ptr, key_len)?;
                let value = read_mem(&mut caller, value_ptr, value_len)?;
                let env = caller.data_mut();
                let is_new = !env.storage.contains_key(&key) && !env.storage_delta.contains_key(&key);
                let gas = if is_new { WASM_SSTORE_NEW_GAS } else { WASM_SSTORE_REWRITE_GAS };
                env.storage_gas = env.storage_gas.saturating_add(gas);
                env.storage_delta.insert(key, Some(value));
                Ok(())
            })
            .map_err(link_err)?;
        linker
            .func_wrap::<(Caller<WasmEnv>, i32, i32), _>("env", "sdelete", |mut caller: Caller<WasmEnv>, key_ptr: i32, key_len: i32| {
                let key = read_mem(&mut caller, key_ptr, key_len)?;
                let env = caller.data_mut();
                env.storage_gas = env.storage_gas.saturating_add(WASM_SDELETE_GAS);
                env.storage_delta.insert(key, None);
                Ok(())
            })
            .map_err(link_err)?;
        // Model X cross-contract call (ADR-016 / Phase 9). The guest passes the
        // call arguments as linear-memory slices + immediates:
        // `call(target, entry_point, payload, ref_height, ref_hash) -> len`.
        // Returns the callee result's byte length (written to `out`) on success,
        // or `0` on **any** failure — no call context, non-UTF-8 entry point,
        // negative ref height, no memory, or any deterministic [`CallFailure`].
        // A call never traps the caller: the failure is data the guest observes.
        linker
            .func_wrap::<(Caller<WasmEnv>, i32, i32, i32, i32, i32, i32, i32, i32, i32, i32, i32), _>(
                "env", "call",
                |mut caller: Caller<WasmEnv>,
                 target_ptr: i32, target_len: i32,
                 entry_ptr: i32, entry_len: i32,
                 payload_ptr: i32, payload_len: i32,
                 ref_height: i32,
                 ref_hash_ptr: i32, ref_hash_len: i32,
                 out_ptr: i32, out_cap: i32|
                 -> Result<i32, wasmi::Error> {
                    // 1. Read the arguments (memory faults → deterministic 0, never a trap).
                    let t = read_mem(&mut caller, target_ptr, target_len);
                    let e = read_mem(&mut caller, entry_ptr, entry_len);
                    let p = read_mem(&mut caller, payload_ptr, payload_len);
                    let r = read_mem(&mut caller, ref_hash_ptr, ref_hash_len);
                    let (target, entry, payload, ref_hash) = match (t, e, p, r) {
                        (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
                        _ => return Ok(0),
                    };
                    let entry = match String::from_utf8(entry) {
                        Ok(s) => s,
                        Err(_) => return Ok(0), // non-UTF-8 entry point
                    };
                    if ref_height < 0 {
                        return Ok(0); // refs are 0-based block indices
                    }
                    // 2. No call context = no call capability (fail closed, deterministic).
                    let Some(ctx) = caller.data().call_ctx.clone() else {
                        return Ok(0);
                    };
                    let a_tx = caller.data().tx.clone();
                    let snapshot_ref = SnapshotRef {
                        height: ref_height as u64,
                        block_hash: ref_hash,
                    };
                    // 3. Sub-budget: A's remaining fuel minus the call base cost, so the
                    //    call can never push A past A's cap (ADR-016 Q4).
                    let remaining = caller.get_fuel().unwrap_or(0);
                    let sub = remaining.saturating_sub(XCALL_CALL_BASE_WASM);
                    // 4. The shared ADR-016 call core (resolve at the pin, run B, fold).
                    let outcome = execute_call(&ctx, &a_tx, &target, &entry, &payload, &snapshot_ref, sub);
                    // 5. Charge A: base + B's sub-execution work.
                    caller.data_mut().call_gas = caller
                        .data_mut()
                        .call_gas
                        .saturating_add(XCALL_CALL_BASE_WASM + outcome.b_gas_used());
                    // 6. Deterministic 0 on any failure (observable, never a trap).
                    let super::call::CallOutcome::Success { result_data, .. } = &outcome else {
                        return Ok(0);
                    };
                    if result_data.len() > out_cap as usize {
                        return Ok(0); // doesn't fit the guest buffer: deterministic failure
                    }
                    if write_mem(&mut caller, out_ptr, result_data).is_err() {
                        return Ok(0);
                    }
                    Ok(result_data.len() as i32)
                },
            )
            .map_err(link_err)?;
        Ok(())
    }

    /// Map a `wasmi` runtime error to a [`ContractError`]: out-of-fuel →
    /// [`ContractError::GasExhausted`]; the revert marker → [`ContractError::Reverted`];
    /// any other trap → [`ContractError::Reverted`] (a guest failure).
    fn map_runtime_err(e: &wasmi::Error) -> ContractError {
        if e.as_trap_code() == Some(TrapCode::OutOfFuel) {
            return ContractError::GasExhausted;
        }
        if e.downcast_ref::<RevertSignal>().is_some() {
            return ContractError::Reverted("guest called env.revert()".into());
        }
        ContractError::Reverted(format!("wasm trap: {e}"))
    }
}

/// Validate a Wasm module for execution **or deployment**: the module size cap,
/// well-formedness, the frozen ABI exports (`__alloc` + `execute`), the sandbox
/// (only `env.*` allow-listed imports), and no `f32`/`f64` on the ABI boundary.
/// Returns the compiled [`Module`] and the [`Engine`] on success so the caller
/// can instantiate it (the deployment path discards both and only checks
/// `Ok`/`Err`).
pub fn validate_wasm_module(bytecode: &[u8]) -> Result<(Module, Engine), ContractError> {
    // 1. Module size cap.
    if bytecode.len() > WASM_MAX_MODULE_BYTES {
        return Err(ContractError::BadBytecode(format!(
            "module size {} bytes exceeds {} byte cap",
            bytecode.len(),
            WASM_MAX_MODULE_BYTES
        )));
    }
    // 2. Compile + validate (well-formed WASM).
    let engine = WasmEngine::build_engine()?;
    let module = Module::new(&engine, bytecode)
        .map_err(|e| ContractError::BadBytecode(format!("invalid module: {e}")))?;
    // 3. ABI + sandbox validation.
    WasmEngine::validate_abi(&module)?;
    Ok((module, engine))
}

impl ContractEngine for WasmEngine {
    fn name(&self) -> &'static str {
        "Wasm"
    }

    fn execute(&self, input: &ExecutionInput) -> Result<ExecutionOutput, ContractError> {
        let bytecode = &input.contract.bytecode;

        // 1-3. Size cap + compile + ABI/sandbox validation (shared with the
        // deployment path). Returns the validated module + engine for
        // instantiation.
        let (module, engine) = validate_wasm_module(bytecode)?;

        // 4. Store with the fuel budget + env snapshot (read-only fields + the W3
        // state-tier scratch: base storage from the input, empty write-set).
        let env = WasmEnv {
            tx: input.tx.clone(),
            tx_amount: input.tx.amount.unwrap_or(0) as i64,
            tx_sequence: input.tx.sequence_number as i64,
            sender_fuel: input.sender_state.fuel_balance as i64,
            payload: input.tx.payload.clone(),
            storage: input.storage.clone(),
            storage_delta: BTreeMap::new(),
            storage_gas: 0,
            call_ctx: input.call_ctx.clone(),
            call_gas: 0,
        };
        let mut store = Store::new(&engine, env);
        let budget = if input.gas_limit == 0 {
            UNLIMITED_FUEL
        } else {
            input.gas_limit
        };
        store
            .set_fuel(budget)
            .map_err(|e| ContractError::InvalidInput(format!("set_fuel: {e}")))?;

        // 5. Link the host imports.
        let mut linker = Linker::<WasmEnv>::new(&engine);
        Self::link_env(&mut linker)?;

        // 6. Instantiate (+ run the module's start function, if any).
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| Self::map_runtime_err(&e))?;

        // 7. ABI I/O.
        let in_bytes = input
            .canonical_bytes()
            .map_err(|e| ContractError::InvalidInput(e.to_string()))?;

        let alloc: wasmi::TypedFunc<i32, i32> = instance
            .get_typed_func(&store, "__alloc")
            .map_err(|e| ContractError::BadBytecode(format!("__alloc: {e}")))?;

        let memory: Memory = instance
            .get_export(&store, "memory")
            .and_then(Extern::into_memory)
            .ok_or_else(|| ContractError::BadBytecode("module has no linear memory".into()))?;

        let in_ptr = alloc
            .call(&mut store, in_bytes.len() as i32)
            .map_err(|e| Self::map_runtime_err(&e))?;
        {
            let mut data = memory.data_mut(&mut store);
            data[in_ptr as usize..in_ptr as usize + in_bytes.len()]
                .copy_from_slice(&in_bytes);
        }

        let out_ptr = alloc
            .call(&mut store, WASM_OUTPUT_CAP as i32)
            .map_err(|e| Self::map_runtime_err(&e))?;

        let execute_fn: wasmi::TypedFunc<(i32, i32, i32, i32), i32> = instance
            .get_typed_func(&store, "execute")
            .map_err(|e| ContractError::BadBytecode(format!("execute: {e}")))?;

        let out_len = execute_fn
            .call(
                &mut store,
                (
                    in_ptr,
                    in_bytes.len() as i32,
                    out_ptr,
                    WASM_OUTPUT_CAP as i32,
                ),
            )
            .map_err(|e| Self::map_runtime_err(&e))?;

        if (out_len as u64) > WASM_OUTPUT_CAP as u64 {
            return Err(ContractError::Reverted(format!(
                "output length {out_len} exceeds {} byte cap",
                WASM_OUTPUT_CAP
            )));
        }

        let module_output = {
            let data = memory.data(&store);
            data[out_ptr as usize..out_ptr as usize + out_len as usize].to_vec()
        };

        // 8. Extract the W3 state-tier results (write-set + storage gas) from the
        // per-call env, then enforce the per-contract storage cap. The cap is checked
        // on the post-apply state (base ∘ delta); exceeding it reverts the call, so no
        // oversized state is ever committed (ADR-018 / Phase 7, QD-gas-cap).
        let (storage_delta, storage_gas, call_gas) = {
            let env = store.data_mut();
            (
                std::mem::take(&mut env.storage_delta),
                env.storage_gas,
                env.call_gas,
            )
        };
        let post_state: BTreeMap<Vec<u8>, Vec<u8>> = {
            let mut s = input.storage.clone();
            for (k, v) in &storage_delta {
                match v {
                    Some(val) => {
                        s.insert(k.clone(), val.clone());
                    }
                    None => {
                        s.remove(k);
                    }
                }
            }
            s
        };
        let post_size = post_state.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
        if post_size > WASM_STORAGE_CAP {
            return Err(ContractError::Reverted(format!(
                "storage size {} bytes exceeds {} byte cap",
                post_size, WASM_STORAGE_CAP
            )));
        }

        // 9. The canonical Wasm result envelope: module output + storage delta. This
        // is the `result_data` the executor stores on the tx and the committer decodes
        // (the delta is not re-derivable from the tx — QD-transport).
        let wasm_result = WasmResult {
            module_output,
            storage_delta,
        };
        let result_data = serialize_to_bytes_rmp(&wasm_result)
            .map_err(|e| ContractError::InvalidInput(format!("serialize WasmResult: {e}")))?;

        // 10. Fuel → gas, plus the W3 storage gas and the Model X call gas (the
        // `env.call` base + each callee's sub-execution work, ADR-016 Q4). The
        // module's fuel budget is `gas_limit` (when declared), so a call whose
        // total (fuel + storage + call) exceeds the declared cap is gas-exhausted
        // (→ the executor's `Failed`, no vote).
        let remaining = store
            .get_fuel()
            .map_err(|e| ContractError::InvalidInput(format!("get_fuel: {e}")))?;
        let fuel_used = budget.saturating_sub(remaining);
        let gas_used = fuel_used
            .saturating_add(storage_gas)
            .saturating_add(call_gas);
        if input.gas_limit > 0 && gas_used > input.gas_limit {
            return Err(ContractError::GasExhausted);
        }

        // 11. Defensive memory cap (fuel metering is the primary bound).
        let mem_size = memory.data_size(&store);
        if mem_size > WASM_MAX_MEMORY_BYTES {
            return Err(ContractError::BadBytecode(format!(
                "linear memory {} bytes exceeds {} byte cap",
                mem_size, WASM_MAX_MEMORY_BYTES
            )));
        }

        Ok(ExecutionOutput::new(result_data, gas_used))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        ContractEngineRegistry, PinnedTarget, SpecEngine, TargetStateProvider,
        validate_snapshot_ref,
    };
    use crate::tokens::{SmartContract, Token};
    use crate::transactions::Transaction;
    use crate::user::User;

    // Embedded fixture modules (built by `wasm_fixtures/compile.sh`; committed so tests
    // run without a wasm toolchain).
    const SUM_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_sum.wasm");
    const ENV_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_env.wasm");
    const REVERT_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_revert.wasm");
    const FORBIDDEN_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_forbidden.wasm");
    const FP_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_fp.wasm");
    const LOOP_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_loop.wasm");
    const STORAGE_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_storage.wasm");
    const STORAGE_BIG_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_storage_big.wasm");
    const SLOAD_ONLY_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_sload_only.wasm");
    const CALLER_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_caller.wasm");

    fn token() -> Token {
        let mut t = Token::new();
        t.id = vec![1, 2, 3];
        t.metadata
            .insert("token_type".to_string(), "contract".to_string());
        t
    }

    fn tx(amount: Option<u64>) -> Transaction {
        Transaction {
            id: "wasm-tx".to_string(),
            action: "ContractCall".to_string(),
            token_id: vec![1, 2, 3],
            bid: None,
            sequence_number: 7,
            sender: vec![9, 8],
            receiver: vec![],
            amount,
            timestamp: 1_700_000_000,
            result_hash: vec![],
            sender_signature: vec![],
            payload: vec![1, 2, 3, 4],
            gas_limit: 1234,
            result_data: vec![],
        }
    }

    fn contract(bytecode: &[u8]) -> SmartContract {
        SmartContract {
            name: "wasm-contract".to_string(),
            bytecode: bytecode.to_vec(),
            version: "1".to_string(),
            storage: Default::default(),
            owners: vec![],
            threshold: 0,
        }
    }

    fn user() -> User {
        User {
            public_key: vec![9, 8],
            fuel_balance: 1000,
            stake: 5,
            nonce: 7,
        }
    }

    /// Build an `ExecutionInput` for `bytecode` and run it on the `WasmEngine`.
    fn run(
        bytecode: &[u8],
        amount: Option<u64>,
        gas_limit: u64,
    ) -> Result<ExecutionOutput, ContractError> {
        let engine = WasmEngine;
        let t = token();
        let tx = tx(amount);
        let c = contract(bytecode);
        let u = user();
        let input = ExecutionInput {
            tx: &tx,
            contract: &c,
            sender_state: &u,
            token: &t,
            gas_limit,
            storage: Default::default(),
            call_ctx: None,
        };
        engine.execute(&input)
    }

    /// Run and return just the `result_data` bytes.
    fn run_data(bytecode: &[u8], amount: Option<u64>, gas_limit: u64) -> Vec<u8> {
        run(bytecode, amount, gas_limit).expect("execute").result_data
    }

    /// Decode the canonical [`WasmResult`] envelope from a Wasm `result_data`.
    fn decode_wasm_result(result_data: &Vec<u8>) -> WasmResult {
        crate::encoding::deserialize_rmp_to(result_data).expect("decode WasmResult")
    }

    // -- W1: pure computation, exact canonical output --------------------------
    #[test]
    fn w1_sum_produces_exact_canonical_output() {
        let engine = WasmEngine;
        let t = token();
        let tx = tx(Some(50));
        let c = contract(SUM_WASM);
        let u = user();
        let input = ExecutionInput {
            tx: &tx,
            contract: &c,
            sender_state: &u,
            token: &t,
            gas_limit: 0,
            storage: Default::default(),
            call_ctx: None,
        };
        let out = engine.execute(&input).expect("w1 sum");
        // The module sums every input byte (mod 2^32) and emits it as a little-endian u32.
        // W3: the `result_data` is the canonical `WasmResult` envelope (module output +
        // empty storage delta), so decode it before comparing the module's raw output.
        let canon = input.canonical_bytes().unwrap();
        let expected_sum: u32 = canon
            .iter()
            .map(|&b| b as u32)
            .fold(0, |acc, b| acc.wrapping_add(b));
        let wr = decode_wasm_result(&out.result_data);
        assert_eq!(wr.module_output, expected_sum.to_le_bytes().to_vec());
        assert!(wr.storage_delta.is_empty(), "sum module writes no storage");
        assert!(out.gas_used > 0);
    }

    // -- W2: read-only host function -------------------------------------------
    #[test]
    fn w2_env_reads_tx_amount() {
        let out = run(ENV_WASM, Some(123_456), 0).expect("w2 env");
        // W3: decode the envelope; the module's raw output is the tx_amount as a LE i64.
        let wr = decode_wasm_result(&out.result_data);
        assert_eq!(wr.module_output.len(), 8);
        let amt = i64::from_le_bytes(wr.module_output.as_slice().try_into().unwrap());
        assert_eq!(amt, 123_456);
        assert!(wr.storage_delta.is_empty());
    }

    // -- W3: storage tier -------------------------------------------------------
    /// Run `STORAGE_WASM` and return the decoded envelope.
    fn run_storage() -> WasmResult {
        let out = run(STORAGE_WASM, Some(50), 0).expect("storage exec");
        decode_wasm_result(&out.result_data)
    }

    // store "a"="hello", load (5B), store "a"="hi" (rewrite), load (2B, RYW),
    // delete "a", load (0B) → output [5,h,e,l,l,o,2,h,i,0]; delta {"a": None}.
    #[test]
    fn w3_storage_round_trip_and_delta() {
        let wr = run_storage();
        let expected_output = [5u8, b'h', b'e', b'l', b'l', b'o', 2, b'h', b'i', 0];
        assert_eq!(wr.module_output, expected_output.to_vec());
        let mut expected_delta: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        expected_delta.insert(b"a".to_vec(), None); // final op is the tombstone
        assert_eq!(wr.storage_delta, expected_delta);
    }

    // The envelope (module output + delta) is deterministic: identical inputs produce
    // byte-identical `result_data` (→ identical `result_hash`). Cross-executor safe.
    #[test]
    fn w3_storage_delta_is_canonical_deterministic() {
        let a = run(STORAGE_WASM, Some(50), 0).expect("run a");
        let b = run(STORAGE_WASM, Some(50), 0).expect("run b");
        assert_eq!(a.result_data, b.result_data);
        assert_eq!(a.gas_used, b.gas_used);
    }

    // `sload` reads the contract's base state (passed via `ExecutionInput::storage`),
    // not just the write-set. A read-only module emits the base value; no delta.
    #[test]
    fn w3_storage_reads_base_state() {
        let engine = WasmEngine;
        let t = token();
        let tx = tx(Some(50));
        let c = contract(SLOAD_ONLY_WASM);
        let u = user();
        let mut base = BTreeMap::new();
        base.insert(b"existing".to_vec(), b"base-value".to_vec());
        let input = ExecutionInput {
            tx: &tx,
            contract: &c,
            sender_state: &u,
            token: &t,
            gas_limit: 0,
            storage: base,
            call_ctx: None,
        };
        let out = engine.execute(&input).expect("sload base");
        let wr = decode_wasm_result(&out.result_data);
        assert_eq!(wr.module_output, b"base-value".to_vec());
        assert!(wr.storage_delta.is_empty(), "read-only module writes no storage");
    }

    // Storage ops charge gas: 1 new sstore (20000) + 1 rewrite (5000) + 1 sdelete
    // (5000) + 3 sloads (100) = 30300 storage gas, on top of the wasmi fuel.
    #[test]
    fn w3_storage_gas_is_charged() {
        let out = run(STORAGE_WASM, Some(50), 0).expect("storage gas");
        assert!(
            out.gas_used >= 30_300,
            "storage gas not charged (got {})",
            out.gas_used
        );
    }

    // A module whose post-apply state exceeds the 1 MiB cap reverts (no oversized
    // state is committed).
    #[test]
    fn w3_storage_cap_exceeded_reverts() {
        let err = run(STORAGE_BIG_WASM, Some(50), 0).unwrap_err();
        assert!(
            matches!(err, ContractError::Reverted(_)),
            "expected Reverted on cap, got: {err:?}"
        );
    }

    // -- Revert ----------------------------------------------------------------
    #[test]
    fn revert_maps_to_reverted() {
        let err = run(REVERT_WASM, Some(50), 0).unwrap_err();
        assert!(matches!(err, ContractError::Reverted(_)), "got: {err:?}");
    }

    // -- Sandbox: disallowed import --------------------------------------------
    #[test]
    fn disallowed_import_rejected() {
        let err = run(FORBIDDEN_WASM, Some(50), 0).unwrap_err();
        assert!(matches!(err, ContractError::BadBytecode(_)), "got: {err:?}");
    }

    // -- Sandbox: f32/f64 disallowed on the ABI boundary -----------------------
    #[test]
    fn fp_exports_disallowed() {
        let err = run(FP_WASM, Some(50), 0).unwrap_err();
        assert!(matches!(err, ContractError::BadBytecode(_)), "got: {err:?}");
    }

    // -- Malformed module -------------------------------------------------------
    #[test]
    fn malformed_module_rejected() {
        // Valid magic+version header, then garbage → not a well-formed module.
        let bad = [0x00u8, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0xFF, 0xFF];
        let err = run(&bad, Some(50), 0).unwrap_err();
        assert!(matches!(err, ContractError::BadBytecode(_)), "got: {err:?}");
    }

    // -- Module size cap ---------------------------------------------------------
    #[test]
    fn module_too_large_rejected() {
        let big = vec![0u8; WASM_MAX_MODULE_BYTES + 1];
        let err = run(&big, Some(50), 0).unwrap_err();
        assert!(matches!(err, ContractError::BadBytecode(_)), "got: {err:?}");
    }

    // -- Fuel exhaustion → GasExhausted ----------------------------------------
    #[test]
    fn fuel_exhaustion_maps_to_gas_exhausted() {
        let err = run(LOOP_WASM, Some(50), 1000).unwrap_err();
        assert!(matches!(err, ContractError::GasExhausted), "got: {err:?}");
    }

    // -- Gas accounting ----------------------------------------------------------
    #[test]
    fn gas_used_is_measured_and_within_limit() {
        let out = run(SUM_WASM, Some(50), 100_000).expect("sum w/ gas");
        assert!(out.gas_used > 0);
        assert!(out.gas_used <= 100_000);
    }

    // -- Cross-executor determinism (ADR-008) -----------------------------------
    #[test]
    fn cross_executor_determinism() {
        // Same module + same input bytes must yield identical `result_data` on a plain
        // thread, a current-thread runtime, and a multi-thread runtime.
        let plain = run_data(SUM_WASM, Some(50), 0);
        let current_thread = {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async { run_data(SUM_WASM, Some(50), 0) })
        };
        let multi_thread = {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async { run_data(SUM_WASM, Some(50), 0) })
        };
        assert_eq!(plain, current_thread);
        assert_eq!(plain, multi_thread);
        assert!(!plain.is_empty());
    }

    // -- Model X: Wasm → Wasm call round-trip (ADR-016, Phase 9) ----------------
    //
    // The caller fixture (`wasm_caller.wasm`) calls `env.call` with hardcoded
    // arguments: target token 0xBB, entry "execute", payload [1,2,3,4], and the
    // B chain's deterministic genesis ref (baked into the fixture). The provider
    // serves the `wasm_sum` fixture as the callee at that pin.

    /// Deterministic block (fixed timestamp + canonical test tx): the B chain's
    /// genesis hash is a test constant — identical on every run and every host.
    fn det_block(prev: Vec<u8>) -> crate::blocks::Block {
        use crate::blocks::{Block, BlockFactory, FinalityStatus};
        use crate::transactions::SignedTransaction;
        use std::collections::HashMap;
        let mut b = Block {
            signed_trans: SignedTransaction::test_transaction(),
            token_metadata: HashMap::new(),
            previous_hash: prev,
            current_hash: vec![],
            timestamp: 1_700_000_000,
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        b.current_hash = BlockFactory::create_hash(&b).expect("well-formed block hashes");
        b
    }

    /// Test provider: serves the pinned target for token `0xbb` (validating the
    /// ref against the target's chain), fails closed for any other token.
    struct WasmCallProvider {
        target: PinnedTarget,
    }

    impl TargetStateProvider for WasmCallProvider {
        fn resolve(
            &self,
            target_token: &[u8],
            snapshot_ref: &SnapshotRef,
            _sender_key: &[u8],
        ) -> Result<PinnedTarget, ContractError> {
            if target_token != [0xbb] {
                return Err(ContractError::InvalidInput("unknown target token".into()));
            }
            validate_snapshot_ref(&self.target.token.blockchain, snapshot_ref)
                .map_err(ContractError::InvalidInput)?;
            Ok(PinnedTarget {
                snapshot_ref: snapshot_ref.clone(),
                ..self.target.clone()
            })
        }
    }

    fn wasm_call_ctx() -> CallContext {
        let b0 = det_block(vec![]);
        let mut b_token = Token::new();
        b_token.id = vec![0xbb];
        b_token
            .metadata
            .insert("token_type".to_string(), "contract".to_string());
        b_token
            .metadata
            .insert("contract_engine".to_string(), "Wasm".to_string());
        b_token.blockchain.add_block(b0);
        let b_contract = contract(SUM_WASM);
        let provider = WasmCallProvider {
            target: PinnedTarget {
                token: b_token,
                contract: b_contract,
                sender_state: user(),
                snapshot_ref: SnapshotRef {
                    height: 0,
                    block_hash: vec![],
                },
            },
        };
        let registry = {
            let r = ContractEngineRegistry::new();
            r.register(Arc::new(WasmEngine));
            r.register(Arc::new(SpecEngine));
            Arc::new(r)
        };
        CallContext::new(Arc::new(provider), registry)
    }

    fn run_caller(ctx: Option<CallContext>) -> ExecutionOutput {
        let t = token();
        let c = contract(CALLER_WASM);
        let u = user();
        let x = tx(None);
        let input = ExecutionInput {
            tx: &x,
            contract: &c,
            sender_state: &u,
            token: &t,
            gas_limit: 0,
            storage: Default::default(),
            call_ctx: ctx.map(Arc::new),
        };
        WasmEngine.execute(&input).expect("caller runs")
    }

    #[test]
    fn wasm_call_wasm_round_trip() {
        let out = run_caller(Some(wasm_call_ctx()));
        let env: WasmResult =
            crate::encoding::deserialize_rmp_to(&out.result_data).expect("outer envelope");
        // Success: the callee's own WasmResult envelope comes back as the caller's
        // module output (not the single-byte 0 failure marker).
        assert!(env.module_output.len() > 4, "envelope too small: {:?}", env.module_output);
        let inner: WasmResult =
            crate::encoding::deserialize_rmp_to(&env.module_output.clone().into())
                .expect("inner envelope");
        assert_eq!(inner.module_output.len(), 4, "wasm_sum emits 4 LE bytes");
        // The caller is charged at least the call base cost.
        assert!(out.gas_used >= XCALL_CALL_BASE_WASM);

        // Cross-executor determinism: an independently built context yields
        // byte-identical output (the pin is the only B state involved).
        let out2 = run_caller(Some(wasm_call_ctx()));
        assert_eq!(out.result_data, out2.result_data);
        assert_eq!(out.gas_used, out2.gas_used);
    }

    #[test]
    fn wasm_call_without_context_returns_failure_marker() {
        // No call context = no call capability: `env.call` returns 0, the fixture
        // emits the single-byte 0 marker, and A settles deterministically.
        let out = run_caller(None);
        let env: WasmResult =
            crate::encoding::deserialize_rmp_to(&out.result_data).expect("outer envelope");
        assert_eq!(env.module_output, vec![0]);
    }

}
