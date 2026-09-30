---
id: concept-contract-scanner
title: "Deploy-time contract scanner — deterministic malware pre-screen (Spec well-formedness + Wasm static walk + canary)"
type: concept
namespace: pneumatic
visibility: namespace
summary: "A pure, fail-closed deploy-time scanner (src/contracts/scan.rs) that runs in DeployValidationSpec (check 4b) and fails closed on a Reject finding (ContractScanFailed): Spec AST well-formedness, a wasmparser Wasm static walk (closes the f32/f64 ABI-boundary gap), and a canary execute on a fixed canonical input + fuel budget."
auto_inject: false
applicable_when: "Working on contract deployment validation, the contract scanner, deploy-time security screening, the Wasm static walk, or the canary run"
confidence: 0.95
verified_at: "09/29/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when scan.rs adds/removes a ScanCode or Severity, when a cap (CANARY_FUEL_BUDGET, SPEC_MAX_INSTRUCTIONS, WASM_MAX_*) is retuned, when the scanner moves out of DeployValidationSpec, or when S1 (P7 storage scan) / S2 (P9 cross-contract scan) land"
tags: [contract-scanner, deployment, security, determinism, fail-closed, wasm, wasmparser, canary, adr-015]
edges:
  - target: decision-contract-deployment
    type: derived_from
    weight: 0.9
    note: "An extension of ADR-015's deploy validation: the scanner is check 4b in DeployValidationSpec, between the Wasm module check and the nonce check"
  - target: decision-wasm-engine-tier2
    type: depends_on
    weight: 0.85
    note: "The Wasm static walk (wasmparser) inspects the same frozen-ABI, integer-only modules the WasmEngine executes; it re-affirms the env.* import allow-list"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.9
    note: "Ambiguity is a Reject, never a silent pass; a missing engine at the canary fails closed"
  - target: decision-executor-pure-read-only
    type: related_to
    weight: 0.8
    note: "Consensus-safe by construction: a pure function of (engine, bytecode) + frozen caps, so the verdict is identical on every shard member (ADR-008)"
  - target: task-executor-contract-execution
    type: supports
    weight: 0.7
    note: "Defense-in-depth pre-screen for the contract-execution plan; S1 (P7 storage scan) and S2 (P9 cross-contract scan) are deferred"
related: ["[[plans/contract-scanner-design.md]]"]
source_url: "plans/contract-scanner-design.md"
---

# Deploy-time contract scanner — deterministic malware pre-screen

`src/contracts/scan.rs` (re-exported from `contracts.rs`; `mod scan`) is a **deterministic,
pure, fail-closed** scanner that runs at deploy validation (`DeployValidationSpec`, check 4b —
between the `validate_wasm_module` gate and the nonce check) to reject obvious malware /
attack vectors *before* a contract is deployed and starts costing gas. It is **defense-in-depth
on top of the sandbox**: the sandbox + fuel + caps are the guarantee a contract *cannot* do
harm; the scanner is the cheap pre-screen. Plan: `plans/contract-scanner-design.md` (S0 landed
09/29/2026; S1 P7-storage and S2 P9-cross-contract deferred).

**Consensus-safety (ADR-008).** Every component is a pure function of `(engine, bytecode)` plus
protocol-frozen constants — no network, no RNG, no clock, no probabilistic ML. The consensus-
critical output is the set of `(ScanCode, Severity)` pairs; the `ScanFinding::detail` string is
audit text and is *not* part of the consensus hash. Cross-executor determinism is tested
(plain / tokio current-thread / tokio multi-thread byte-identical findings).

**Finding model.** `Severity { Reject, Warn }` (Reject fails the deploy closed; Warn is
log-only in v1, QDS5). `ScanCode` classifies each finding. A `Reject` maps to
`ValidationFailureReason::ContractScanFailed`.

**Three components, dispatched by engine (`scan_contract`):**

1. **Spec well-formedness** (`scan_spec`) — the `Spec` ISA is a closed, straight-line AST (no
   loops/branches/calls), so this is near-decidable: rmp-decode to `InstructionProgram`
   (`SpecBadBytecode` if it fails), version == 1, instruction count ≤ `SPEC_MAX_INSTRUCTIONS`
   (1000, the explicit `Spec` gas bound), and an exact stack-depth walk (underflow → `Warn`,
   it reverts safely at runtime; unknown `LoadTx` field → `Warn`).
2. **Wasm static walk** (`scan_wasm`, via `wasmparser = "=0.239.0"`, `default-features = false`) —
   `wasmi::Module` exposes only the header, so opcode-level checks parse the binary directly.
   Imports must be `env.*` allow-list only (`WasmBadImport`, Reject — re-affirms the runtime
   sandbox); **internal `f32`/`f64` opcodes → `WasmFloatOpcode` (Reject)** — detected via the
   operator's `Debug` name prefix so *any* float variant (incl. SIMD) is caught, closing the gap
   the integer-only *ABI-boundary* check misses (fixture `wasm_fp_internal.wasm`); non-ABI exports
   → `Warn`; `memory.grow` count, data-section and custom-section sizes, loop nesting, and
   op/function complexity are `Warn` review triggers.
3. **Canary run** (`canary_run`) — a deterministic deploy-time smoke test: one
   `engine.execute()` on a **fixed canonical** `ExecutionInput` (no live state) with
   `gas_limit = CANARY_FUEL_BUDGET` (1,000,000). `GasExhausted` → `CanaryGasExhausted` (Reject,
   gas-burn vector); output > `CANARY_MAX_OUTPUT` (4 KiB) → `CanaryOutputOverflow` (Reject,
   output-spam); `Reverted` is **accepted** (a legitimate outcome, not malice); a missing engine
   fails closed.

**Caps** (protocol-tunable, frozen per chain): `CANARY_FUEL_BUDGET = 1_000_000`,
`CANARY_MAX_OUTPUT = 4 KiB`, `SPEC_MAX_INSTRUCTIONS = 1_000`, `WASM_MAX_MEMORY_GROW = 64`,
`WASM_MAX_DATA_BYTES = 64 KiB`, `WASM_MAX_CUSTOM_BYTES = 16 KiB`, `WASM_MAX_LOOP_NESTING = 2`.

**Honest limits.** Wasm is Turing-complete, so no scanner gives a complete malware guarantee —
the sandbox + fuel + wall-clock are the real guarantee. The scanner is a pre-screen that catches
obvious vectors (floats, disallowed imports, gas-burn loops, output spam, malformed `Spec`) cheaply
and deterministically. The re-entrancy treatment for P9 (cross-contract) is deferred to S2.
