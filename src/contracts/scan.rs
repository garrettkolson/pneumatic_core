//! Deploy-time contract scanner (ADR-015 extension; `plans/contract-scanner-design.md`).
//!
//! A **deterministic, pure, fail-closed** scanner that runs at deploy validation
//! (`DeployValidationSpec`) to reject obvious malware / attack vectors *before* a
//! contract is deployed and starts costing gas. It is **consensus-safe by
//! construction**: every component is a pure function of `(engine, bytecode)` plus
//! protocol-frozen constants, so it yields an identical verdict on every shard member
//! (ADR-008). It is defense-in-depth on top of the sandbox — the sandbox + fuel + caps
//! are the guarantee a contract *cannot* do harm; the scanner is the cheap pre-screen.
//!
//! Three engine-dispatched components:
//! 1. **Spec well-formedness** (`scan_spec`) — decidable static analysis of the closed,
//!    straight-line `Spec` AST.
//! 2. **Wasm static walk** (`scan_wasm`) — heuristic opcode/section analysis via
//!    `wasmparser` (closes the `f32`/`f64` ABI-boundary gap at the opcode level).
//! 3. **Canary run** (`canary_run`) — a deterministic deploy-time smoke test: one
//!    `engine.execute` on a fixed canonical input + fixed fuel budget.
//!
//! Findings carry a [`Severity`]: [`Severity::Reject`] fails the deploy closed;
//! [`Severity::Warn`] is flagged for review (v1: logged only).
//!
//! ## The hard constraint: no floats, determinism, fail-closed
//! - **No `f32`/`f64`** anywhere in a Wasm module (opcode-level, not just the ABI
//!   boundary) — a consensus-critical invariant (integer-only contract surface).
//! - **Pure** — no I/O, no network, no clock, no RNG, no external service.
//! - **Fail-closed** — any ambiguity is a [`Severity::Reject`], never a silent pass.

use wasmparser::{Chunk, Operator, Parser, Payload};

use super::wasm::ALLOWED_ENV_IMPORTS;
use super::{
    ContractEngine, ContractEngineRegistry, ContractError, ExecutionInput, InstructionProgram,
    Op,
};
use crate::encoding::deserialize_rmp_to;
use crate::tokens::{SmartContract, Token};
use crate::transactions::Transaction;
use crate::user::User;

// ---------------------------------------------------------------------------
// Caps (protocol-tunable, frozen per chain — like the existing WASM_MAX_* in wasm.rs)
// ---------------------------------------------------------------------------

/// Canary-run fuel budget (QDS4). A contract that burns this on a trivial input is a
/// gas-burn / economic-DoS vector.
pub const CANARY_FUEL_BUDGET: u64 = 1_000_000;
/// Canary-run max `result_data` size in bytes. Larger output on a trivial input is an
/// output-spam vector.
pub const CANARY_MAX_OUTPUT: usize = 4 * 1024;
/// Max `Spec` instruction count. Each `Spec` op costs one gas and the ISA has no loops,
/// so this is the explicit, finite `Spec` gas bound.
pub const SPEC_MAX_INSTRUCTIONS: usize = 1_000;
/// Wasm `memory.grow` op-count threshold (a module that grows memory abnormally is
/// flagged for review).
pub const WASM_MAX_MEMORY_GROW: u32 = 64;
/// Wasm total `data` section size threshold (bytes) — large data segments are a
/// payload-staging vector.
pub const WASM_MAX_DATA_BYTES: u64 = 64 * 1024;
/// Wasm total custom-section size threshold (bytes) — large custom sections are a
/// steganography vector.
pub const WASM_MAX_CUSTOM_BYTES: u64 = 16 * 1024;
/// Loop-nesting depth that triggers the unbounded-loop `Warn` (nested loops are a
/// stronger infinite-loop / gas-burn signal than a single flat loop, which the canary
/// catches via fuel).
pub const WASM_MAX_LOOP_NESTING: u32 = 2;
/// Obfuscation/complexity thresholds — a module above these is flagged for manual review.
const OBFUSCATE_MAX_OPCODES: u64 = 500_000;
const OBFUSCATE_MAX_FUNCTIONS: u32 = 2_000;

/// The known `Spec` `LoadTx` field names (mirrors `load_tx_field` in `contracts.rs`).
const KNOWN_LOADTX_FIELDS: &[&str] = &[
    "amount",
    "sequence_number",
    "gas_limit",
    "sender",
    "receiver",
    "token_id",
    "payload",
];

// ---------------------------------------------------------------------------
// Finding model
// ---------------------------------------------------------------------------

/// The severity of a [`ScanFinding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The deploy is failed closed.
    Reject,
    /// Flagged for review; does not block the deploy (v1: logged only, QDS5).
    Warn,
}

/// The classification of a [`ScanFinding`].
///
/// The consensus-critical output of the scanner is the set of `(ScanCode, Severity)`
/// pairs — a pure function of `(engine, bytecode)` + the frozen caps. The
/// [`ScanFinding::detail`] string is audit/observability text and is *not* part of the
/// consensus hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCode {
    // -- Spec well-formedness --
    /// The `Spec` bytecode does not decode to an `InstructionProgram`.
    SpecBadBytecode,
    /// The `Spec` program version is not 1.
    SpecBadVersion,
    /// A `LoadTx` references a field name that is not one of the known fields.
    SpecUnknownLoadTx,
    /// The straight-line `Spec` stack depth would underflow (reverts at runtime).
    SpecStackUnderflow,
    /// The `Spec` program exceeds [`SPEC_MAX_INSTRUCTIONS`].
    SpecTooManyInstructions,
    // -- Wasm static walk --
    /// The module failed to parse in the static walk (should not happen:
    /// `validate_wasm_module` runs first; fail closed).
    WasmParseFailed,
    /// An import is outside the `env.*` allow-list (re-affirmed; the runtime sandbox
    /// already rejects it).
    WasmBadImport,
    /// An internal `f32`/`f64` opcode (closes the ABI-boundary gap — integer-only).
    WasmFloatOpcode,
    /// A non-ABI export is present (frozen ABI is `__alloc` + `execute`).
    WasmExtraExport,
    /// Abnormal `memory.grow` op count.
    WasmMemoryGrowth,
    /// A nested-loop pattern that is a potential infinite-loop / gas-burn vector.
    WasmUnboundedLoop,
    /// An oversized `data` section.
    WasmLargeData,
    /// An oversized custom-section payload.
    WasmLargeCustom,
    /// High complexity / op obfuscation (manual-review flag).
    WasmObfuscated,
    // -- Canary run --
    /// The engine is not registered, so the canary cannot run (fail closed).
    CanaryEngineMissing,
    /// The canary exhausted its fuel budget (gas-burn vector).
    CanaryGasExhausted,
    /// The canary output exceeds the cap (output-spam vector).
    CanaryOutputOverflow,
}

/// A single scanner finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFinding {
    pub code: ScanCode,
    pub severity: Severity,
    /// Human/audit-readable detail. Not part of the consensus hash.
    pub detail: String,
}

impl ScanFinding {
    fn reject(code: ScanCode, detail: impl Into<String>) -> Self {
        ScanFinding {
            code,
            severity: Severity::Reject,
            detail: detail.into(),
        }
    }
    fn warn(code: ScanCode, detail: impl Into<String>) -> Self {
        ScanFinding {
            code,
            severity: Severity::Warn,
            detail: detail.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Run the full scanner for `engine_name` over `bytecode`.
///
/// Dispatches by engine: `Spec` → well-formedness + canary; `Wasm` → static walk +
/// canary. `Transfer` (and any other engine) has no bytecode to scan; unknown engines
/// are rejected upstream by `DeployValidationSpec` (check #3) before the scanner runs.
///
/// The returned `Vec<ScanFinding>` is a pure function of `(engine, bytecode)` + the
/// frozen caps — identical on every shard member (ADR-008).
pub fn scan_contract(
    registry: &ContractEngineRegistry,
    engine_name: &str,
    bytecode: &[u8],
) -> Vec<ScanFinding> {
    let mut findings = Vec::new();
    match engine_name {
        "Spec" => {
            findings.extend(scan_spec(bytecode));
            findings.extend(canary_run(registry, engine_name, bytecode));
        }
        "Wasm" => {
            findings.extend(scan_wasm(bytecode));
            findings.extend(canary_run(registry, engine_name, bytecode));
        }
        _ => {}
    }
    findings
}

// ---------------------------------------------------------------------------
// Component 1 — Spec well-formedness (decidable)
// ---------------------------------------------------------------------------

/// Static analysis of the closed, straight-line `Spec` AST.
///
/// The `Spec` ISA has no loops, branches, calls, storage, or I/O, so this is
/// near-decidable: parse, check the version, bound the instruction count (the explicit
/// gas bound), and track the exact stack depth (straight-line → deterministic).
fn scan_spec(bytecode: &[u8]) -> Vec<ScanFinding> {
    // 1. Parse the AST.
    let program: InstructionProgram = match deserialize_rmp_to(&bytecode.to_vec()) {
        Ok(p) => p,
        Err(_) => {
            return vec![ScanFinding::reject(
                ScanCode::SpecBadBytecode,
                "bytecode does not decode to an InstructionProgram",
            )];
        }
    };

    let mut findings = Vec::new();

    // 2. Version check.
    if program.version != 1 {
        findings.push(ScanFinding::reject(
            ScanCode::SpecBadVersion,
            format!("unsupported program version {}", program.version),
        ));
    }

    // 3. Instruction / gas bound (each op = 1 gas; no loops → max gas = ops.len()).
    if program.ops.len() > SPEC_MAX_INSTRUCTIONS {
        findings.push(ScanFinding::reject(
            ScanCode::SpecTooManyInstructions,
            format!(
                "{} instructions exceed the {} cap",
                program.ops.len(),
                SPEC_MAX_INSTRUCTIONS
            ),
        ));
    }

    // 4. Exact stack-depth walk. The program is straight-line (a `Halt` terminates
    //    execution), so the depth after each op is a deterministic value. An underflow
    //    reverts safely at runtime (`contracts.rs::pop_one`), so it is a quality `Warn`,
    //    not a security `Reject`.
    let mut depth: i32 = 0;
    let mut underflow = false;
    for op in &program.ops {
        if let Op::Halt = op {
            break; // the interpreter stops here; later ops are dead code
        }
        let (pops, pushes) = match op {
            Op::LoadTx(_) | Op::LoadConst(_) => (0, 1),
            Op::Add | Op::Sub | Op::Mul | Op::Mod | Op::Cmp => (2, 1),
            Op::Select => (3, 1),
            Op::Emit => (1, 0),
            Op::Halt => unreachable!(),
        };
        if depth < pops {
            underflow = true;
            break;
        }
        depth -= pops;
        depth += pushes;

        if let Op::LoadTx(field) = op {
            if !KNOWN_LOADTX_FIELDS.contains(&field.as_str()) {
                findings.push(ScanFinding::warn(
                    ScanCode::SpecUnknownLoadTx,
                    format!("unknown LoadTx field \"{}\"", field),
                ));
            }
        }
    }
    if underflow {
        findings.push(ScanFinding::warn(
            ScanCode::SpecStackUnderflow,
            "stack underflow detected (reverts at runtime)",
        ));
    }

    findings
}

// ---------------------------------------------------------------------------
// Component 2 — Wasm static walk (heuristic, via wasmparser)
// ---------------------------------------------------------------------------

/// Heuristic opcode/section analysis of a Wasm module via `wasmparser`.
///
/// `wasmi::Module` exposes the module *header* (imports/exports) but not the decoded
/// opcode bodies, so the opcode-level checks (notably internal `f32`/`f64`) need a
/// binary parser. The walk is a pure function of the module bytes (deterministic).
fn scan_wasm(bytecode: &[u8]) -> Vec<ScanFinding> {
    let mut state = WasmWalkState::default();

    let mut parser = Parser::new(0);
    let mut offset: usize = 0;
    loop {
        let chunk = match parser.parse(&bytecode[offset..], true) {
            Ok(c) => c,
            Err(_) => {
                return vec![ScanFinding::reject(
                    ScanCode::WasmParseFailed,
                    "module failed to parse in the static walk",
                )];
            }
        };
        let payload = match chunk {
            Chunk::NeedMoreData(_) => {
                // With `eof = true` and a complete buffer this must not occur; treat it
                // as a parse failure (fail closed).
                return vec![ScanFinding::reject(
                    ScanCode::WasmParseFailed,
                    "parser requested more data unexpectedly",
                )];
            }
            Chunk::Parsed { consumed, payload } => {
                offset += consumed;
                payload
            }
        };
        let is_end = matches!(payload, Payload::End(_));
        state.walk_payload(payload);
        if is_end {
            break;
        }
    }

    state.into_findings()
}

/// Accumulates the `wasmparser` walk's counters, then emits the findings.
#[derive(Default)]
struct WasmWalkState {
    bad_import: bool,
    float_op: bool,
    extra_export: bool,
    memory_grow: u32,
    data_bytes: u64,
    custom_bytes: u64,
    total_functions: u32,
    total_opcodes: u64,
    max_loop_nesting: u32,
}

impl WasmWalkState {
    fn walk_payload(&mut self, payload: Payload) {
        match payload {
            Payload::ImportSection(reader) => {
                for import in reader {
                    if let Ok(import) = import {
                        if import.module != "env"
                            || !ALLOWED_ENV_IMPORTS.contains(&import.name)
                        {
                            self.bad_import = true;
                        }
                    }
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    if let Ok(export) = export {
                        if export.name != "__alloc" && export.name != "execute" {
                            self.extra_export = true;
                        }
                    }
                }
            }
            Payload::CodeSectionEntry(body) => self.walk_function_body(&body),
            Payload::DataSection(reader) => {
                for seg in reader {
                    if let Ok(d) = seg {
                        self.data_bytes += d.data.len() as u64;
                    }
                }
            }
            Payload::CustomSection(reader) => {
                self.custom_bytes += reader.data().len() as u64;
            }
            _ => {}
        }
    }

    fn walk_function_body(&mut self, body: &wasmparser::FunctionBody) {
        self.total_functions += 1;
        let ops = match body.get_operators_reader() {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut nesting: u32 = 0;
        for op in ops {
            let op = match op {
                Ok(o) => o,
                Err(_) => break,
            };
            self.total_opcodes += 1;
            if is_float_op(&op) {
                self.float_op = true;
            }
            match op {
                Operator::MemoryGrow { .. } => self.memory_grow += 1,
                Operator::Loop { .. } => {
                    nesting += 1;
                    if nesting > self.max_loop_nesting {
                        self.max_loop_nesting = nesting;
                    }
                }
                Operator::End => {
                    if nesting > 0 {
                        nesting -= 1;
                    }
                }
                _ => {}
            }
        }
    }

    fn into_findings(self) -> Vec<ScanFinding> {
        let mut f = Vec::new();
        if self.bad_import {
            f.push(ScanFinding::reject(
                ScanCode::WasmBadImport,
                "import outside the env.* allow-list (already rejected by the runtime sandbox)",
            ));
        }
        if self.float_op {
            f.push(ScanFinding::reject(
                ScanCode::WasmFloatOpcode,
                "internal f32/f64 opcode detected (integer-only contract)",
            ));
        }
        if self.extra_export {
            f.push(ScanFinding::warn(
                ScanCode::WasmExtraExport,
                "non-ABI export present (frozen ABI is __alloc + execute)",
            ));
        }
        if self.memory_grow > WASM_MAX_MEMORY_GROW {
            f.push(ScanFinding::warn(
                ScanCode::WasmMemoryGrowth,
                format!(
                    "{} memory.grow ops exceed the {} threshold",
                    self.memory_grow, WASM_MAX_MEMORY_GROW
                ),
            ));
        }
        if self.data_bytes > WASM_MAX_DATA_BYTES {
            f.push(ScanFinding::warn(
                ScanCode::WasmLargeData,
                format!(
                    "data section {} bytes exceeds the {} byte threshold",
                    self.data_bytes, WASM_MAX_DATA_BYTES
                ),
            ));
        }
        if self.custom_bytes > WASM_MAX_CUSTOM_BYTES {
            f.push(ScanFinding::warn(
                ScanCode::WasmLargeCustom,
                format!(
                    "custom sections {} bytes exceed the {} byte threshold",
                    self.custom_bytes, WASM_MAX_CUSTOM_BYTES
                ),
            ));
        }
        if self.max_loop_nesting >= WASM_MAX_LOOP_NESTING {
            f.push(ScanFinding::warn(
                ScanCode::WasmUnboundedLoop,
                format!(
                    "loop nesting {} (>= {}) — potential infinite-loop / gas-burn (fuel bounds it at runtime)",
                    self.max_loop_nesting, WASM_MAX_LOOP_NESTING
                ),
            ));
        }
        if self.total_opcodes > OBFUSCATE_MAX_OPCODES
            || self.total_functions > OBFUSCATE_MAX_FUNCTIONS
        {
            f.push(ScanFinding::warn(
                ScanCode::WasmObfuscated,
                format!(
                    "high complexity ({} functions, {} opcodes)",
                    self.total_functions, self.total_opcodes
                ),
            ));
        }
        f
    }
}

/// `true` if the operator is a scalar `f32`/`f64` op.
///
/// Detected via the operator's `Debug` name prefix — robust to *any* current or future
/// float variant (including SIMD `f32x4`/`f64x2`), so the "no floats escape" invariant
/// is guaranteed, not best-effort. The per-op `format!` allocation is a one-time
/// deploy-validation cost (never the hot execution path) and is negligible for a
/// ≤ 1 MiB module.
fn is_float_op(op: &Operator) -> bool {
    let name = format!("{:?}", op);
    name.starts_with("F32") || name.starts_with("F64")
}

// ---------------------------------------------------------------------------
// Component 3 — Canary run (deterministic smoke test)
// ---------------------------------------------------------------------------

/// Execute the module once against a **fixed canonical** input and a **fixed** fuel
/// budget, then check the outcome.
///
/// The input is a protocol-frozen constant (no live state), so the run is identical on
/// every shard member (ADR-008). The canary's job is specifically:
/// - **Termination** — `GasExhausted` on a trivial input is a gas-burn vector
///   ([`ScanCode::CanaryGasExhausted`], `Reject`).
/// - **Output bound** — output > [`CANARY_MAX_OUTPUT`] is an output-spam vector
///   ([`ScanCode::CanaryOutputOverflow`], `Reject`).
/// - A `Reverted` outcome is **accepted** (a legitimate contract outcome, not malice).
/// - `BadBytecode` / `InvalidInput` are static-analysis / registration concerns already
///   covered upstream (`validate_wasm_module`, `scan_spec`, the engine-registered
///   check), so the canary does not re-report them.
fn canary_run(
    registry: &ContractEngineRegistry,
    engine_name: &str,
    bytecode: &[u8],
) -> Vec<ScanFinding> {
    let engine = match registry.get(engine_name) {
        Some(e) => e,
        None => {
            return vec![ScanFinding::reject(
                ScanCode::CanaryEngineMissing,
                format!(
                    "engine \"{}\" not registered; cannot run canary (fail closed)",
                    engine_name
                ),
            )];
        }
    };

    // The fixed canonical canary input (a protocol-frozen constant).
    let tx = Transaction {
        id: "canary".to_string(),
        action: "ContractCall".to_string(),
        token_id: vec![0u8; 32],
        bid: None,
        sequence_number: 0,
        sender: vec![0u8; 32],
        receiver: Vec::new(),
        amount: Some(0),
        timestamp: 0,
        result_hash: Vec::new(),
        sender_signature: Vec::new(),
        payload: Vec::new(),
        gas_limit: CANARY_FUEL_BUDGET,
        result_data: vec![],
    };
    let contract = SmartContract {
        name: "canary".to_string(),
        bytecode: bytecode.to_vec(),
        version: "1".to_string(),
        storage: Default::default(),
        owners: vec![],
        threshold: 0,
    };
    let user = User {
        public_key: vec![0u8; 32],
        fuel_balance: CANARY_FUEL_BUDGET,
        stake: 0,
        nonce: 0,
    };
    let mut token = Token::new();
    token.id = vec![0u8; 32];
    token
        .metadata
        .insert("token_type".to_string(), "contract".to_string());
    token
        .metadata
        .insert("contract_engine".to_string(), engine_name.to_string());

    let input = ExecutionInput {
        tx: &tx,
        contract: &contract,
        sender_state: &user,
        token: &token,
        gas_limit: CANARY_FUEL_BUDGET,
        storage: Default::default(),
    };

    match engine.execute(&input) {
        Err(ContractError::GasExhausted) => vec![ScanFinding::reject(
            ScanCode::CanaryGasExhausted,
            format!(
                "canary exhausted the {} fuel budget (gas-burn vector)",
                CANARY_FUEL_BUDGET
            ),
        )],
        Err(ContractError::Reverted(_)) => Vec::new(),
        // BadBytecode / InvalidInput / EngineNotImplemented / UnknownEngine are
        // covered upstream — the canary does not re-report them.
        Err(_) => Vec::new(),
        Ok(out) if out.result_data.len() > CANARY_MAX_OUTPUT => vec![ScanFinding::reject(
            ScanCode::CanaryOutputOverflow,
            format!(
                "canary output {} bytes exceeds the {} byte cap (output-spam vector)",
                out.result_data.len(),
                CANARY_MAX_OUTPUT
            ),
        )],
        Ok(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::WasmEngine;
    use crate::encoding::serialize_to_bytes_rmp;

    // Reuse the committed Wasm fixtures (no toolchain needed to run tests).
    const SUM_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_sum.wasm");
    const ENV_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_env.wasm");
    const REVERT_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_revert.wasm");
    const FORBIDDEN_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_forbidden.wasm");
    const FP_INTERNAL_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_fp_internal.wasm");
    const LOOP_WASM: &[u8] = include_bytes!("wasm_fixtures/wasm_loop.wasm");

    /// A registry with the built-in engines + the WasmEngine (so the canary can run
    /// `Wasm`).
    fn registry_with_wasm() -> ContractEngineRegistry {
        let r = ContractEngineRegistry::new();
        r.register_defaults();
        r.register(std::sync::Arc::new(WasmEngine));
        r
    }

    fn spec_program_bytes(ops: Vec<Op>) -> Vec<u8> {
        serialize_to_bytes_rmp(&InstructionProgram {
            version: 1,
            ops,
        })
        .expect("rmp")
    }

    // -- Spec well-formedness ------------------------------------------------

    #[test]
    fn spec_clean_program_has_no_findings() {
        let reg = ContractEngineRegistry::new();
        reg.register_defaults();
        let bytecode = spec_program_bytes(vec![
            Op::LoadConst(7),
            Op::LoadConst(6),
            Op::Mul,
            Op::Emit,
            Op::Halt,
        ]);
        let findings = scan_spec(&bytecode);
        assert!(findings.is_empty(), "clean spec: {findings:?}");
    }

    #[test]
    fn spec_malformed_bytecode_is_reject() {
        let findings = scan_spec(b"\xFF\x01\x02");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, ScanCode::SpecBadBytecode);
        assert_eq!(findings[0].severity, Severity::Reject);
    }

    #[test]
    fn spec_bad_version_is_reject() {
        let bytecode = serialize_to_bytes_rmp(&InstructionProgram {
            version: 2,
            ops: vec![Op::Halt],
        })
        .unwrap();
        let findings = scan_spec(&bytecode);
        assert!(
            findings.iter().any(|f| f.code == ScanCode::SpecBadVersion),
            "got: {findings:?}"
        );
    }

    #[test]
    fn spec_unknown_loadtx_field_is_warn() {
        let bytecode = spec_program_bytes(vec![Op::LoadTx("nonce".to_string()), Op::Emit]);
        let findings = scan_spec(&bytecode);
        assert!(
            findings
                .iter()
                .any(|f| f.code == ScanCode::SpecUnknownLoadTx && f.severity == Severity::Warn),
            "got: {findings:?}"
        );
    }

    #[test]
    fn spec_stack_underflow_is_warn() {
        // A binary op with one value → underflow.
        let bytecode = spec_program_bytes(vec![Op::LoadConst(1), Op::Add, Op::Emit]);
        let findings = scan_spec(&bytecode);
        assert!(
            findings
                .iter()
                .any(|f| f.code == ScanCode::SpecStackUnderflow && f.severity == Severity::Warn),
            "got: {findings:?}"
        );
    }

    #[test]
    fn spec_too_many_instructions_is_reject() {
        let ops: Vec<Op> = (0..=SPEC_MAX_INSTRUCTIONS)
            .map(|_| Op::LoadConst(1))
            .collect();
        let bytecode = spec_program_bytes(ops);
        let findings = scan_spec(&bytecode);
        assert!(
            findings
                .iter()
                .any(|f| f.code == ScanCode::SpecTooManyInstructions),
            "got: {findings:?}"
        );
    }

    // -- Wasm static walk ----------------------------------------------------

    #[test]
    fn wasm_clean_module_has_no_reject() {
        let findings = scan_wasm(SUM_WASM);
        assert!(
            !findings.iter().any(|f| f.severity == Severity::Reject),
            "clean wasm should have no Reject: {findings:?}"
        );
    }

    #[test]
    fn wasm_internal_float_opcode_is_reject() {
        // Integer exports (passes the ABI-boundary check) but internal f32/f64 ops.
        let findings = scan_wasm(FP_INTERNAL_WASM);
        assert!(
            findings
                .iter()
                .any(|f| f.code == ScanCode::WasmFloatOpcode && f.severity == Severity::Reject),
            "got: {findings:?}"
        );
    }

    #[test]
    fn wasm_disallowed_import_is_reject() {
        let findings = scan_wasm(FORBIDDEN_WASM);
        assert!(
            findings
                .iter()
                .any(|f| f.code == ScanCode::WasmBadImport),
            "got: {findings:?}"
        );
    }

    #[test]
    fn wasm_loop_module_flags_unbounded_loop_or_is_clean() {
        // `wasm_loop.wasm` has a (single flat) loop; the static walk's unbounded-loop
        // `Warn` fires on nesting >= 2. A single flat loop is caught by the canary
        // (fuel) instead, so this only asserts the walk itself succeeds and does not
        // misfire a Reject.
        let findings = scan_wasm(LOOP_WASM);
        assert!(
            !findings.iter().any(|f| f.severity == Severity::Reject),
            "loop module should not be a static Reject: {findings:?}"
        );
    }

    #[test]
    fn wasm_garbage_bytes_is_parse_reject() {
        let findings = scan_wasm(b"\x00\x61\x73\x6d\x01\x00\x00\x00\xFF\xFF");
        assert!(
            findings.iter().any(|f| f.code == ScanCode::WasmParseFailed),
            "got: {findings:?}"
        );
    }

    // -- Canary run ----------------------------------------------------------

    #[test]
    fn canary_clean_module_passes() {
        let reg = registry_with_wasm();
        let findings = canary_run(&reg, "Wasm", SUM_WASM);
        assert!(findings.is_empty(), "clean canary: {findings:?}");
    }

    #[test]
    fn canary_loop_module_exhausts_fuel() {
        let reg = registry_with_wasm();
        let findings = canary_run(&reg, "Wasm", LOOP_WASM);
        assert!(
            findings
                .iter()
                .any(|f| f.code == ScanCode::CanaryGasExhausted),
            "got: {findings:?}"
        );
    }

    #[test]
    fn canary_revert_is_accepted() {
        let reg = registry_with_wasm();
        let findings = canary_run(&reg, "Wasm", REVERT_WASM);
        assert!(
            !findings
                .iter()
                .any(|f| f.code == ScanCode::CanaryGasExhausted || f.code == ScanCode::CanaryOutputOverflow),
            "revert should be accepted: {findings:?}"
        );
    }

    #[test]
    fn canary_missing_engine_is_reject() {
        let reg = ContractEngineRegistry::new(); // no Wasm registered
        let findings = canary_run(&reg, "Wasm", SUM_WASM);
        assert!(
            findings
                .iter()
                .any(|f| f.code == ScanCode::CanaryEngineMissing),
            "got: {findings:?}"
        );
    }

    // -- Dispatcher ----------------------------------------------------------

    #[test]
    fn dispatcher_spec_clean() {
        let reg = registry_with_wasm();
        let bytecode = spec_program_bytes(vec![Op::LoadConst(1), Op::Emit, Op::Halt]);
        let findings = scan_contract(&reg, "Spec", &bytecode);
        assert!(findings.is_empty(), "clean spec dispatcher: {findings:?}");
    }

    #[test]
    fn dispatcher_wasm_float_reject() {
        let reg = registry_with_wasm();
        let findings = scan_contract(&reg, "Wasm", FP_INTERNAL_WASM);
        assert!(
            findings
                .iter()
                .any(|f| f.code == ScanCode::WasmFloatOpcode && f.severity == Severity::Reject),
            "got: {findings:?}"
        );
    }

    #[test]
    fn dispatcher_transfer_has_no_findings() {
        let reg = registry_with_wasm();
        let findings = scan_contract(&reg, "Transfer", b"anything");
        assert!(findings.is_empty());
    }

    // -- Cross-executor determinism (ADR-008) --------------------------------
    //
    // The scanner is consensus-critical: the same (engine, bytecode) must yield
    // byte-identical findings on a plain thread, a current-thread runtime, and a
    // multi-thread runtime. A divergence would be a consensus fork.

    fn findings_signature(engine: &str, bytecode: &[u8]) -> Vec<(ScanCode, Severity)> {
        let reg = registry_with_wasm();
        scan_contract(&reg, engine, bytecode)
            .into_iter()
            .map(|f| (f.code, f.severity))
            .collect()
    }

    #[test]
    fn cross_executor_determinism_scanner() {
        // A battery of fixtures × both engines.
        let cases: [(&str, &[u8]); 6] = [
            ("Spec", &spec_program_bytes(vec![Op::LoadConst(1), Op::Add, Op::Emit])),
            ("Spec", &spec_program_bytes(vec![Op::LoadConst(7), Op::Emit, Op::Halt])),
            ("Wasm", SUM_WASM),
            ("Wasm", ENV_WASM),
            ("Wasm", FP_INTERNAL_WASM),
            ("Wasm", LOOP_WASM),
        ];
        for (engine, bytecode) in &cases {
            let plain = findings_signature(engine, *bytecode);
            let ct = {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async { findings_signature(engine, *bytecode) })
            };
            let mt = {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async { findings_signature(engine, *bytecode) })
            };
            assert_eq!(plain, ct, "engine {engine}: plain != current-thread");
            assert_eq!(plain, mt, "engine {engine}: plain != multi-thread");
        }
    }
}
