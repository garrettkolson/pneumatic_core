---
id: log-organize-vault-20260929-084405
type: log
operation: organize-vault
date: "2026-09-29T08:44:05"
namespace: pneumatic
summary: "Phase 5 of the executor contract-execution plan landed: the Tier-2 WasmEngine core (wasmi 1.1.0) is implemented in pneumatic_core with W1+W2 capability tiers, fuel-metered gas, frozen ABI, sandbox, caps, and 10 tests incl. cross-executor determinism; core suite 572 -> 582"
affected_nodes: ["task-executor-contract-execution", "decision-wasm-engine-tier2", "decision-contract-engine-model"]
tags: ["log", "organize-vault"]
---

**Phase 5 (WasmEngine core, Tier-2) is implemented and landed** (ADR-018, QW1–QW6 all
approved). This is the code landing that ADR-018's design was scoped for.

- **New module** `pneumatic_core::contracts::wasm` (`src/contracts/wasm.rs`; `mod wasm`
  + `pub use wasm::WasmEngine` in `src/contracts.rs`). Registry name `"Wasm"`, **opt-in** —
  `register_defaults` still ships only `Transfer` + `Spec`, and `select_engine` is
  unchanged. `wasmi` pinned to **`1.1.0`** with `default-features = false,
  features = ["std"]` to drop the `wat`/`wast`/`wasm-encoder` 2.x dependency chain
  (MSRV 1.88; the toolchain is 1.87) — binary WASM only, never WAT text.
- **Gas = WASM fuel** (ADR-013): `gas_limit` is the fuel budget (`0` ⇒ `u64::MAX`
  unbounded); an out-of-fuel trap (`TrapCode::OutOfFuel`) → `ContractError::GasExhausted`;
  `gas_used = budget − get_fuel()`.
- **Frozen ABI**: the module exports `__alloc(size)->ptr` +
  `execute(input_ptr,input_len,output_ptr,output_cap)->output_len`; the host allocates both
  buffers, writes `ExecutionInput.canonical_bytes()` in, and reads `result_data` out
  (bounds-checked against a 64 KiB output cap).
- **Sandbox**: only the `env` namespace is imported, and only the allow-listed W2
  read-only host fns (`tx_amount`, `tx_sequence`, `sender_fuel`, `tx_payload_len`,
  `tx_payload`, `revert`); any other namespace/import → `BadBytecode` before
  instantiation. `revert` traps with a `HostError` marker → `Reverted`. **`f32`/`f64`
  disallowed** on the ABI boundary (export func/global signatures checked; imports are
  enforced by type-matching against the integer-only host fns). **Caps** (protocol-
  tunable): module ≤ 1 MiB, linear memory ≤ 32 MiB (post-call check; fuel metering is the
  primary bound).
- **Fixtures**: six committed `.wasm` modules under `src/contracts/wasm_fixtures/` (built
  by `compile.sh`, now with `--crate-type cdylib`): W1 sum, W2 `tx_amount`, revert,
  forbidden-import, f32-export, fuel-loop.
- **Tests**: 10 new `contracts::wasm` tests — exact canonical output (W1), W2 host read,
  revert, disallowed import, f32 disallow, malformed module, module-size cap, fuel
  exhaustion → `GasExhausted`, gas accounting, and **cross-executor determinism** (plain /
  tokio current-thread / tokio multi-thread byte-identical). Core suite **572 → 582**; full
  workspace `cargo test` green (executor 20, committer 92+9, finalizer 61, node_server 32,
  sentinel 57).

Remaining for the WasmEngine: **W3** storage (Phase 7), **W4** cross-contract `call`
(Phase 9).
