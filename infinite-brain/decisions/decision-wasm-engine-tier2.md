---
id: decision-wasm-engine-tier2
title: "ADR-018: Tier-2 WasmEngine (wasmi) behind the ContractEngine trait"
type: decision
namespace: pneumatic
visibility: namespace
summary: "WasmEngine is a Tier-2 ContractEngine impl (wasmi interpreter, fuel-metered, sandboxed, frozen ABI) so SmartContract.bytecode can be a WASM module for rich contract logic; opt-in per environment, capability tiers W1 compute → W2 read state → W3 storage → W4 cross-contract."
auto_inject: false
applicable_when: "Designing, scoping, or implementing the Tier-2 WasmEngine, its gas/sandbox model, storage, or cross-contract host functions"
confidence: 0.95
verified_at: "09/29/2026"
verified_by: "Garrett Olson"
staleness_signal: "Core landed (Phase 5, 09/29/2026). Stale when W3 storage (Phase 7) lands and changes the ABI/caps, or when a different WASM runtime is chosen"
tags: [adr, design-decision, contract-execution, engine, wasm, wasmi, determinism, gas, sandbox]
edges:
  - target: decision-contract-engine-model
    type: derived_from
    weight: 0.95
    note: "Realizes ADR-011's Tier-2 WasmEngine; a registry entry, not the substrate"
  - target: decision-executor-pure-read-only
    type: depends_on
    weight: 0.9
    note: "The sandbox computes; writes are a canonical delta in result_data applied at commit (ADR-013)"
  - target: decision-contract-model-lifecycle
    type: depends_on
    weight: 0.85
    note: "Wasm modules deploy/upgrade via the ADR-014 on-chain deployment + governance paths; W4 is the Wasm form of Model X calls"
  - target: decision-deterministic-leader-election
    type: related_to
    weight: 0.6
    note: "Shares the ADR-008 determinism requirement: same module + inputs => same result_hash on every shard member"
  - target: task-executor-contract-execution
    type: supports
    weight: 0.9
    note: "The WasmEngine is Phase 5 (core) + Phase 7 (storage) of the executor contract-execution plan"
related: ["[[plans/wasm-engine-design.md]]"]
source_url: "plans/wasm-engine-design.md"
---

# ADR-018: Tier-2 WasmEngine (wasmi) behind the ContractEngine trait

**Approved by Garrett Olson, 2026-09-28.** Full design: `plans/wasm-engine-design.md`.

**Phase 5 (core) landed 2026-09-29.** `WasmEngine` is implemented in
`pneumatic_core::contracts::wasm` (new `src/contracts/wasm.rs`; registry name `"Wasm"`,
opt-in — not in `register_defaults`). `wasmi` pinned to **`1.1.0`** (`default-features
= false, features = ["std"]` to drop the `wat`/`wast` MSRV-1.88 chain; binary WASM
only). **W1** (pure compute) + **W2** (read-only host fns `tx_amount`, `tx_sequence`,
`sender_fuel`, `tx_payload_len`, `tx_payload`, `revert`) capability tiers are live. Gas
= WASM fuel (`gas_limit` budget, `0` = unbounded; out-of-fuel → `GasExhausted`;
`gas_used = budget − get_fuel()`). Frozen ABI (`__alloc` + `execute`), sandbox (only
`env.*` allow-listed imports), `f32`/`f64` disallowed on the ABI boundary, caps
(module ≤ 1 MiB, memory ≤ 32 MiB). 10 tests incl. cross-executor determinism. Core
572 → 582; workspace `cargo test` green. **W3** storage (Phase 7) and **W4**
cross-contract (Phase 9) remain.

Contract execution gains a **Tier-2** engine, `WasmEngine` (registry name `"Wasm"`),
implementing the existing `ContractEngine` trait so `SmartContract.bytecode` can be a
**WASM module** with general control flow (loops, branching, real arithmetic,
arbitrary-length data) — the "rich contract logic" the closed `u64` `SpecEngine` ISA
cannot express. It is a **registry entry, not the substrate** (ADR-011): no changes to
the trait, `select_engine`, the executor, or the Phase-4 timeout/panic/sandbox plumbing.

**Runtime — `wasmi`** (pure-Rust **interpreter**), with `wasm-instrument` for fuel
metering. Chosen over `wasm3` (native JIT) because the interpreter's determinism is
**structural** (no JIT → no host-dependent codegen) and its sandbox/attack surface is
far smaller — the same reason Polkadot uses `wasmi` for on-chain runtime evaluation.
Both are **exact-pinned** as security-sensitive dependencies.

**Model**
- **Sandbox:** WASM linear memory + capability model; the only host surface is an
  allow-list of fuel-metered host imports. No network/FS/env/clock/RNG.
- **Gas (ADR-013):** WASM **fuel** = gas. `gas_limit` is the fuel budget (`0` = no cap);
  out-of-fuel → `ContractError::GasExhausted` → `Failed`. Memory pages, module size,
  storage size, and call depth are capped; the Phase-4 wall-clock backstop is the safety
  net, fuel is the deterministic consensus bound.
- **Frozen ABI:** the module exports `__alloc` + `execute(input_ptr, input_len) ->
  output_len`; the host writes `ExecutionInput.canonical_bytes()` in and reads
  `result_data` out. The ABI + allowed feature set + imports are **versioned and frozen**
  (consensus-critical).
- **State (ADR-013/C6):** the sandbox is **read-only**; a contract's writes are a
  **canonical delta emitted in `result_data`** (token transfers + a storage delta),
  applied at commit. This keeps the sandbox pure and the writes auditable.

**Capability tiers** (each independently shippable):
- **W1** pure computation (the WasmEngine core — Phase 5).
- **W2** read-only protocol state via host imports (pinned to the execution-time
  snapshot, identical across shard members — Phase 5).
- **W3** per-contract `sload`/`sstore` storage, emitted as a storage delta (Phase 7) —
  what makes it "rich" (order books, registries, counters).
- **W4** cross-contract `call(target, entry, payload, snapshot_ref)` — the Wasm form of
  Model X (Phase 9, ADR-016).

**Opt-in:** `register_defaults` keeps only `Transfer` + `Spec`; `Wasm` is enabled per
environment via the `contract_engines` spec (QW3). `f32`/`f64` are disallowed in Tier-1
modules (QW4) to keep determinism airtight.

**Phasing:** the old "Phase 9 (Optional, Tier-2) Wasm engine" stub is promoted and
restructured — **P5** WasmEngine core, **P6** deployment (engine-agnostic), **P7**
Wasm storage, **P8** governance, **P9** Model X (incl. Wasm `call`), **P10** e2e.

**Decisions locked (2026-09-28):** QW1 runtime = `wasmi`; QW2 storage writes =
emit-a-canonical-delta-in-`result_data`; QW3 `Wasm` opt-in via the env spec; QW4
`f32`/`f64` disallowed (integer-only); QW5 ABI = entrypoint + pointer/length with a
standard `__alloc`; QW6 caps per design §8 (memory 32 MiB, module 1 MiB, storage/depth
protocol-tunable).
