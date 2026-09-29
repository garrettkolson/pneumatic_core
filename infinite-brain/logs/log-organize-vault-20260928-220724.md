---
id: log-organize-vault-20260928-220724
type: log
operation: organize-vault
date: "2026-09-28T22:07:24"
namespace: pneumatic
summary: "Tier-2 WasmEngine designed (ADR-018) and slotted into the executor contract-execution plan as first-class Phases 5 + 7; the old 'Phase 9 (Optional) Wasm' stub is promoted and Phases renumbered to 1-10"
affected_nodes: ["task-executor-contract-execution", "decision-wasm-engine-tier2", "decision-contract-engine-model"]
tags: ["log", "organize-vault"]
---

The **Tier-2 `WasmEngine`** was designed (design-for-approval, not implemented) and the
executor contract-execution plan's future phases were updated to account for it.

- **New design** `plans/wasm-engine-design.md`: a `WasmEngine` (registry name `"Wasm"`)
  implementing the existing `ContractEngine` trait so `SmartContract.bytecode` can be a
  WASM module with general control flow. Runtime = **`wasmi`** (pure-Rust interpreter,
  chosen over `wasm3` for structural determinism + a smaller sandbox). Fuel metering
  (`wasm-instrument`) = ADR-013 gas; frozen `__alloc`/`execute` ABI; sandbox (linear
  memory + capability model); caps (fuel, memory pages, module size, storage, call depth);
  `f32`/`f64` disallowed; `Wasm` is **opt-in** via the `contract_engines` env spec.
  Capability tiers: **W1** pure compute, **W2** read-only host fns, **W3** per-contract
  `sload`/`sstore` storage (writes emitted as a canonical delta in `result_data`, applied
  at commit — ADR-013), **W4** cross-contract `call` (the Wasm form of Model X).
- **New ADR-018** decision node `decision-wasm-engine-tier2` (realizes ADR-011's
  Tier-2 WasmEngine; ADR-011's staleness signal fires when it lands).
- **Plan restructured** (`plans/executor-contract-execution-implementation-plan.md`):
  the old "Phase 9 (Optional, Tier-2) Wasm engine" stub is **promoted** — **P5**
  WasmEngine core, **P6** deployment (engine-agnostic, was P5), **P7** Wasm storage (new),
  **P8** governance (was P6), **P9** Model X incl. the Wasm `call` (was P7), **P10** e2e
  (was P8). Header, Risks (new Wasm-runtime row), Effort estimate (~5–6 weeks), and vault
  duties updated.
- **Task node** `task-executor-contract-execution`: title/summary/checklist → Phases 1-10
  with the WasmEngine phases; new edge to ADR-018; staleness signal updated.
- **INDEX.md**: ADR-018 added to the decision table (18 → 19); task row synced.

Open design decisions for approval: QW1 runtime (wasmi), QW2 storage-write model
(emit-delta), QW3 opt-in, QW4 no FP, QW5 ABI style, QW6 caps. **No code implemented** —
this is the design + plan/vault update only.
