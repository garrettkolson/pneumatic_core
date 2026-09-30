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

use wasmi::{
    Caller, Config, Engine, Extern, ExternType, Instance, Linker, Memory, Module, Store,
    TrapCode, ValType,
};

use super::{ContractEngine, ContractError, ExecutionInput, ExecutionOutput};

// ---------------------------------------------------------------------------
// Caps (ADR-018 QW6) — protocol-tunable.
// ---------------------------------------------------------------------------

/// Maximum compiled-module size in bytes (1 MiB).
pub const WASM_MAX_MODULE_BYTES: usize = 1 * 1024 * 1024;
/// Maximum linear memory in bytes (512 pages = 32 MiB).
pub const WASM_MAX_MEMORY_BYTES: usize = 512 * 64 * 1024;
/// Output buffer the host allocates for a module's `result_data` (64 KiB).
const WASM_OUTPUT_CAP: u32 = 64 * 1024;
/// Fuel budget used when `gas_limit == 0` (effectively unbounded).
const UNLIMITED_FUEL: u64 = u64::MAX;

/// Allow-listed `env.*` host imports (the W2 read-only capability tier + revert).
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
];

// ---------------------------------------------------------------------------
// Host state + revert marker
// ---------------------------------------------------------------------------

/// The read-only, execution-time-snapshot state the `env.*` host functions read.
/// Owned by the [`Store`] so the `'static` host closures (required by
/// [`Linker::func_wrap`]) can reach it via `caller.data()`. Every field is a pure
/// function of [`ExecutionInput`] — identical across shard members (Model-X invariant).
struct WasmEnv {
    tx_amount: i64,
    tx_sequence: i64,
    sender_fuel: i64,
    payload: Vec<u8>,
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

        // 4. Store with the fuel budget + read-only env snapshot.
        let env = WasmEnv {
            tx_amount: input.tx.amount.unwrap_or(0) as i64,
            tx_sequence: input.tx.sequence_number as i64,
            sender_fuel: input.sender_state.fuel_balance as i64,
            payload: input.tx.payload.clone(),
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

        let result_data = {
            let data = memory.data(&store);
            data[out_ptr as usize..out_ptr as usize + out_len as usize].to_vec()
        };

        // 8. Fuel → gas.
        let remaining = store
            .get_fuel()
            .map_err(|e| ContractError::InvalidInput(format!("get_fuel: {e}")))?;
        let gas_used = budget.saturating_sub(remaining);

        // 9. Defensive memory cap (fuel metering is the primary bound).
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
        }
    }

    fn contract(bytecode: &[u8]) -> SmartContract {
        SmartContract {
            name: "wasm-contract".to_string(),
            bytecode: bytecode.to_vec(),
            version: "1".to_string(),
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
        };
        engine.execute(&input)
    }

    /// Run and return just the `result_data` bytes.
    fn run_data(bytecode: &[u8], amount: Option<u64>, gas_limit: u64) -> Vec<u8> {
        run(bytecode, amount, gas_limit).expect("execute").result_data
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
        };
        let out = engine.execute(&input).expect("w1 sum");
        // The module sums every input byte (mod 2^32) and emits it as a little-endian u32.
        let canon = input.canonical_bytes().unwrap();
        let expected_sum: u32 = canon
            .iter()
            .map(|&b| b as u32)
            .fold(0, |acc, b| acc.wrapping_add(b));
        assert_eq!(out.result_data, expected_sum.to_le_bytes().to_vec());
        assert!(out.gas_used > 0);
    }

    // -- W2: read-only host function -------------------------------------------
    #[test]
    fn w2_env_reads_tx_amount() {
        let out = run(ENV_WASM, Some(123_456), 0).expect("w2 env");
        assert_eq!(out.result_data.len(), 8);
        let amt = i64::from_le_bytes(out.result_data.as_slice().try_into().unwrap());
        assert_eq!(amt, 123_456);
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
}
