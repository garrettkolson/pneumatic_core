---
id: log-contract-scanner-s0-20260929-145031
type: log
operation: implement
date: "2026-09-29T14:50:31"
namespace: pneumatic
summary: "Contract scanner S0 landed: src/contracts/scan.rs (Spec well-formedness + wasmparser Wasm static walk + canary run) wired into DeployValidationSpec as check 4b with ContractScanFailed; wasmparser =0.239.0 added; core 601 -> 623"
affected_nodes: ["concept-contract-scanner", "decision-contract-deployment"]
tags: ["log", "implement", "contract-scanner"]
---

The deploy-time contract scanner (S0) landed 2026-09-29, per
`plans/contract-scanner-design.md`. New module `src/contracts/scan.rs` (re-exported from
`contracts.rs`) implements three pure, fail-closed, deterministic components dispatched by engine
via `scan_contract`: (1) `scan_spec` — decidable well-formedness of the closed straight-line
`Spec` AST (rmp-decode, version==1, ≤ `SPEC_MAX_INSTRUCTIONS`=1000, exact stack-depth walk,
unknown-`LoadTx`/underflow → Warn); (2) `scan_wasm` — a `wasmparser` static walk (env-only
allow-listed imports → Reject, **internal f32/f64 opcodes → Reject** via the operator's `Debug`
name prefix, closing the ABI-boundary gap that the integer-only export check misses; non-ABI
exports / memory.grow / data+custom section sizes / loop nesting / complexity → Warn); (3)
`canary_run` — one `engine.execute` on a fixed canonical `ExecutionInput` with
`CANARY_FUEL_BUDGET`=1_000_000 (GasExhausted / output>4 KiB → Reject; Reverted accepted; missing
engine fails closed). Finding model: `Severity{Reject,Warn}`, `ScanCode`, `ScanFinding`
(`detail` is audit-only, not in the consensus hash).

Wiring: `DeployValidationSpec` runs the scanner as check 4b (after `validate_wasm_module`, before
the nonce check); a `Reject` fails the deploy with the new
`ValidationFailureReason::ContractScanFailed` (added to `errors.rs`), `Warn` is logged (v1, QDS5).
`wasmparser = "=0.239.0"` added as a direct dep with `default-features = false` (avoids the
`indexmap/serde 1.0.220` bump that default features would pull; `wasmi` already re-enables
`hash-collections` without serde). `ALLOWED_ENV_IMPORTS` in `wasm.rs` made `pub(crate)` (single
source of truth for the walk). New fixture `wasm_fp_internal.wasm` (integer exports, internal
f32/f64 ops) exercises the ABI-boundary gap.

Tests: 19 scanner unit tests (per-`ScanCode` fixtures, canary, cross-executor determinism —
plain / tokio current-thread / tokio multi-thread byte-identical findings) + 3 deploy integration
tests (malicious Wasm float → `ContractScanFailed`, clean Wasm → accepted, gas-burn loop →
`ContractScanFailed`). Two existing deploy tests (`valid_deploy_passes`, `nonce_mismatch_fails`,
`missing_sender_user_fails`) updated to use a valid `InstructionProgram` (the scanner now
rejects the old `b"ast-bytes"` placeholder). Workspace `cargo test` green: **core 623** (was 601),
committer 95+9, executor 21, finalizer 61, node-server 32, prover 15, sentinel 57.

Deferred (S1/S2): P7 Wasm-storage scan and P9 cross-contract / re-entrancy scan (re-entrancy
treatment — non-atomic snapshot-pinned calls see stale pre-call state — is documented in the
plan's P9 notes, item 2 + canary probe + QDS7).
