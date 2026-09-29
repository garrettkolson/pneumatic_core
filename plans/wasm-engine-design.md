# WasmEngine Design — Tier-2 Contract Engine

Status: **design for approval** (2026-09-28). Promotes the plan's stub
"Phase 9 — (Optional, Tier-2) Wasm engine" to a first-class, designed phase.
Companion ADR: **ADR-018** (`infinite-brain/decisions/decision-wasm-engine-tier2.md`).
Builds on ADR-011 (pluggable `ContractEngine`), ADR-013 (pure read-only executor,
deltas at commit), ADR-014 (contract ≡ token, Model X calls), ADR-008 (determinism
is consensus-critical).

The goal: support **rich contract logic** (beyond the `u64` straight-line `SpecEngine`
ISA) behind the *existing* `ContractEngine` trait — deterministically, sandboxed,
gas-bounded — without changing the executor, the registry, or per-token selection.

---

## 1. Goals & non-goals

**Goals**
- A `WasmEngine` implementing `ContractEngine` so `SmartContract.bytecode` can be a
  WASM module with general control flow (loops, branching, real 32/64-bit arithmetic,
  arbitrary-length data).
- **Determinism by construction**: identical `result_hash` on every executor in a
  shard (ADR-008). No JIT, no host-dependent codegen.
- **Safety**: hard sandbox; a malicious module can compute and burn its gas, but cannot
  escape memory isolation, touch the host, or hang the worker.
- **Bounded resources**: gas (WASM fuel) is the deterministic consensus bound; memory
  and module size are capped; the Phase-4 wall-clock backstop is the safety net.
- **Reuse**: the existing deployment (Phase 6), upgrade-governance (Phase 8), and
  Model-X cross-contract (Phase 9) paths apply to Wasm modules with no special-casing.

**Non-goals**
- Not an EVM clone; not a drop-in for *arbitrary* WASM — a restricted, frozen feature
  set (no networking, no filesystem, no host env, no threads, no `bulk-memory`/`reference-types`
  features in Tier-1).
- Not the substrate. `Transfer`/`Spec` remain the default Tier-1 engines; `Wasm` is an
  opt-in registry entry (ADR-011).
- No re-architecting the `ContractEngine` trait — the WasmEngine fits `fn execute(&self,
  input) -> Result<ExecutionOutput, _>` as-is.

---

## 2. Runtime choice: `wasmi` (pure-Rust interpreter)

**Recommended: [`wasmi`](https://github.com/wasmi/wasmi)** — a pure-Rust WASM
interpreter, with [`wasm-instrument`](https://crates.io/crates/wasm-instrument)
for fuel metering.

| Property | Why it matters here |
|---|---|
| **Interpreter, no JIT** | Byte-identical results on every host/CPU/arch. A JIT (e.g. `wasm3`) compiles to native code — host-dependent codegen + a much larger native attack surface. For consensus, the interpreter's determinism is *structural*, not policy-maintained. This is exactly why Polkadot uses `wasmi` + `wasm-instrument` for on-chain runtime evaluation. |
| **WASM sandbox** | Linear memory + capability model: the module cannot read/write host memory, call arbitrary host fns, or do I/O unless the host explicitly links an import. |
| **First-class fuel metering** | WASM "fuel" charges per instruction; maps 1:1 onto ADR-013 gas (`gas_limit` = fuel budget, `gas_used` = fuel consumed, out-of-fuel → `GasExhausted`). |
| **Pure Rust, auditable** | Small, reviewable dependency; no C/FFI in the hot path. |
| **`no-std`-friendly** | Fits the executor's embedded context. |

**Rejected: `wasm3`** (native 32-bit JIT). Faster, but JIT determinism is a consensus
risk (codegen can vary by toolchain/host), and shipping native machine code from an
untrusted module into the node process is a far larger safety surface than an
interpreter. Speed is a non-goal for Tier-1 (the gas + wall-clock bounds already cap
runtime); correctness and auditability win.

**Dependency handling**: `wasmi` + `wasm-instrument` are **security-sensitive** (they
are the sandbox). Per the repo convention, they are **exact-pinned** in the workspace
`Cargo.toml`; a version bump is an API-migration + re-audit event, not a routine update.
(Version selected at implementation; the design is runtime-agnostic between `wasmi`
releases as long as the frozen ABI holds.)

**Determinism notes**
- WASM has no implicit nondeterminism (no timing, no RNG, no I/O) — good by default.
- **Floating point** (`f32`/`f64`): `wasmi` implements FP in pure Rust, so it is
  deterministic *for a given wasmi version*. To remove any doubt in Tier-1, the
  validator **disallows `f32`/`f64`** in modules by default (see QW4); modules are
  integer-only. This also matches the rest of the protocol (all `u64`/bytes).

---

## 3. The contract as a WASM module

- `SmartContract.bytecode` = the **WASM module bytes** (WASM binary format).
- `Token.metadata["contract_engine"] = "Wasm"`, `token_type = "contract"`.
- `select_engine` is **unchanged** — it is already generic (name → registry). No
  executor changes.
- **Validation at deployment** (Phase 6): the module must be (a) well-formed (WASM
  validation passes), (b) within the **module size cap** (see §8), (c) exporting the
  ABI entrypoint (§4), (d) importing **only** the allow-listed host functions (§7).
  Any failure → the `DeployContract` tx fails closed.
- **Freezing**: the ABI (§4) and the allowed WASM feature set + host imports are
  **versioned and frozen**. Changing them is a hard fork (they are consensus-critical:
  the same module must produce the same `result_hash` on all executors forever).

---

## 4. The ABI (host ↔ module interface)

The frozen interface between the protocol and a Wasm module. Recommended convention
(**QW5**): an **entrypoint + pointer/length** ABI, with a standard wasm allocator.

The module must export:
```
__alloc(size: i32) -> i32          // allocate `size` bytes in linear memory, return ptr
execute(input_ptr: i32, input_len: i32) -> i32   // run; return output_len
```
Host protocol (inside `WasmEngine::execute`, on the Phase-4 blocking thread):
1. `in_bytes = input.canonical_bytes()` (the existing deterministic input — unchanged).
2. `in_ptr = __alloc(len(in_bytes))`; write `in_bytes` at `in_ptr`.
3. Set the fuel budget to `input.gas_limit` (or ∞ if `gas_limit == 0`); run
   `execute(in_ptr, len)`. On out-of-fuel → `ContractError::GasExhausted`.
4. The module writes its result into a buffer it allocated (returned as `output_len`;
   the output starts at a fixed `output_ptr` convention — see QW5). Read `output_len`
   bytes → `result_data`.
5. `gas_used` = fuel consumed; return `ExecutionOutput { result_data, gas_used }`.
   The executor then computes `result_hash = hash(result_data)` (unchanged).

**Revert**: the module signals a revert via a non-zero exit convention (a reserved
`execute` return-code range) or a `revert(msg)` host import → `ContractError::Reverted`.

`__alloc`/`execute` are the *only* required exports; the host provides the input and
reads the output. This keeps the module's memory management self-contained (it owns its
allocator) and the host trivially auditable.

---

## 5. Gas model (ADR-013 alignment)

WASM **fuel** is the gas mechanism. Mapping:

| ADR-013 stage | WasmEngine realization |
|---|---|
| Stage 1 — sender-declared cap | `input.gas_limit` is the **fuel budget** (`0` = no cap, matching the other engines). |
| Stage 2 — engine metered cost | `wasm-instrument` charges **fuel per instruction** as the module runs; `gas_used` = fuel consumed, always `≤ gas_limit` on success. |
| Stage 3 — commit-time accounting | `gas_used` flows into `ExecutionResult` and is deducted at commit (unchanged from the other engines). |

- **Out of fuel** → `ContractError::GasExhausted` → tx `Failed` (no vote), exactly like
  `SpecEngine`.
- **Memory gas**: linear-memory page growth is charged (each 64 KiB page costs fuel);
  a **hard max-memory-page cap** bounds memory (see §8).
- **Host functions** are individually fuel-metered (§7) — a state read costs fuel, a
  storage write costs more, a cross-contract `call` transfers a sub-budget.
- **Wall-clock backstop** (Phase 4) is the *safety net* against a bug that defeats fuel
  metering; **fuel is the deterministic, consensus-relevant bound** (it is what makes
  two executors agree on `gas_used`).

---

## 6. Sandbox & security

- **Memory isolation**: WASM linear memory; the module cannot touch host memory.
- **Capability model**: the *only* host surface is the allow-listed imports (§7). No
  networking, filesystem, environment, clocks, or randomness are linked.
- **Caps** (all enforced at validation + runtime):
  - Fuel (gas) — the computation bound.
  - **Max memory pages** (e.g. 512 pages = 32 MiB; protocol-tunable).
  - **Module size cap** on `bytecode` (e.g. 1 MiB; protocol-tunable) — bounds deployment
    and memory.
- **Wall-clock backstop** (Phase 4) bounds real time; **`spawn_blocking`** (Phase 4)
  keeps a heavy module off the async worker.
- A malicious module's worst case: burn its gas and its memory cap, then fail. It cannot
  escape the sandbox, read host state, or hang the node (fuel + wall-clock bound it).

---

## 7. Capability tiers (how "rich" gets real)

A pure WASM module with no host imports is a *very* powerful `SpecEngine` (loops,
branching, real data) but is **stateless** — it can only transform its input. To be
genuinely rich, the engine gains **host functions** (WASM imports), added in tiers.
Each tier is independently shippable; each host function is pure, deterministic, and
fuel-metered.

- **W1 — Pure computation (the WasmEngine core).** The module computes over
  `ExecutionInput` (tx fields + calldata) and emits a **canonical state delta** in
  `result_data` (the existing ADR-013 pattern — `TransferEngine` already does this with
  a `TransferDelta`). No host imports. Stateless, deterministic, off-chain testable.
  *This is the minimum viable Tier-2 and lands first (Phase 5).*

- **W2 — Read-only protocol state.** Host imports let the module **read** deterministic
  state, e.g. `get_balance(token, account) -> u64`, `get_token_metadata(token)`,
  `get_stake_set()`. Enriches computation without adding state. All reads are **pinned to
  the execution-time state snapshot**, which is identical across shard members (the same
  invariant Model X relies on) — so determinism is preserved.

- **W3 — Per-contract storage (`sload`/`sstore`).** A persistent key/value store scoped
  to the contract token. This is what enables order books, registries, counters,
  whitelists, etc.
  - Storage lives in the contract token's state (a new `Token`/data-service field; the
    storage is *scoped to the token*, so it stays within ADR-014 C6's "own token only"
    effect scope).
  - **Writes are not applied in the sandbox.** `sstore` is recorded by the host into a
    **storage delta** (ordered `(key, value)` pairs + tombstones for deletes). At the end
    of execution the storage delta is rmp-canonicalized and **emitted in `result_data`**
    alongside any token-transfer delta. The committer applies it at commit (idempotent,
    exactly like the `CreateToken` delta). The sandbox stays pure: it *computes* and the
    host *records intent*; consensus *applies*. This is ADR-013 made explicit for Wasm.
  - Gas: `sload` costs fuel; `sstore` costs more (new key > overwrite); a **per-contract
    storage size cap** bounds it.

- **W4 — Cross-contract calls (Model X).** Host import
  `call(target_token, entry_point, payload, snapshot_ref) -> (result, ok)`. The host
  resolves target B's state at the pinned snapshot ref `(height, hash)`, validates the
  ref against B's chain, and runs B's engine in the same call frame under a sub-gas
  budget. This is the **Wasm form of Phase 9's Model-X `Call`** (the `SpecEngine` gets
  the same capability as an ISA `Call` op). Composes with W3. Lands in Phase 9 (ADR-016).

---

## 8. Caps & limits (protocol-tunable, frozen per chain)

| Limit | Purpose | Suggested default |
|---|---|---|
| Fuel budget | Computation bound | tx `gas_limit` |
| Max memory pages | Memory DoS bound | 512 pages (32 MiB) |
| Module size cap | Deployment + memory bound | 1 MiB |
| Per-contract storage cap | State DoS bound (W3) | protocol-tunable |
| `call` nesting depth (W4) | Recursion DoS bound | small (e.g. 8) |
| Wall-clock backstop | Safety net (Phase 4) | 5 s (`PNEUMATIC_EXECUTOR_TIMEOUT_SECS`) |

---

## 9. Relationship to the other engines

- `Transfer` (simple value movement), `Spec` (u64 straight-line AST), `Wasm` (full
  module). They **coexist** in the `ContractEngineRegistry`; a token picks one via the
  `contract_engine` metadata key. No changes to `select_engine` or the executor.
- `register_defaults` registers only `Transfer` + `Spec` (unchanged). **`Wasm` is
  opt-in** via the environment `contract_engines` spec (see QW3) — existing deployments
  do not suddenly accept WASM.
- The `SpecEngine` remains the cheap, auditable path for simple contracts; the
  `WasmEngine` is the rich path.

---

## 10. Deployment & lifecycle (reuses existing phases)

- **Deployment (Phase 6)** is engine-agnostic: a `DeployContract` tx stores the WASM
  module as the token's `asset_data` (bytecode) + `contract_engine = "Wasm"`. The
  deployment validator runs the §3 validation (well-formed, size, ABI exports, allowed
  imports). Deterministic token id unchanged (C2).
- **Upgrades (Phase 8)** = replace `asset_data` (the module) under M-of-N + timelock
  (C5). A Wasm upgrade swaps in a new module; the frozen ABI guarantees old and new
  modules speak the same interface.
- **Cross-contract (Phase 9)** = the W4 `call` host import (Model X, ADR-016).

---

## 11. What changes in the codebase

- **New** `WasmEngine` (likely `src/contracts/wasm.rs`) implementing `ContractEngine`.
- **New deps** `wasmi` + `wasm-instrument` (exact-pinned, security-sensitive).
- **New** host-function module (the capability surface) — grows W2 → W3 → W4.
- **Extend** the token/data-service model with a per-contract **storage field** (W3) +
  a **storage-delta** schema in `result_data`.
- **Extend** `validate_execution_result` with a Wasm path (validate the emitted
  token-transfer + storage delta).
- **Environment spec**: `contract_engines` may include `"Wasm"`.
- **No change** to the `ContractEngine` trait, `select_engine`, the executor, or the
  Phase-4 timeout/panic/sandbox plumbing.

---

## 12. Determinism risks & mitigations

| Risk | Mitigation |
|---|---|
| JIT host-dependent codegen | Use the **interpreter** (`wasmi`) — no JIT. |
| Floating-point variance | **Disallow `f32`/`f64`** in Tier-1 modules (QW4); integer-only. |
| State-read divergence across shards | All reads **pinned to the execution-time snapshot**, identical across shard members (Model-X invariant). |
| Host-function nondeterminism | Host fns are **pure, deterministic, fuel-metered, and audited**; they are the only capability surface. |
| A module that defeats fuel metering | **Wall-clock backstop** (Phase 4) as the safety net. |
| Module that changes behavior across wasmi versions | **Pin** the wasmi version; the frozen ABI + a **canary test corpus** (fixed modules → fixed `result_hash`) regression-guard the runtime. |

A **cross-executor determinism test** (two `Executor`s, same Wasm module + inputs →
identical `result_hash` and `"Sign"` votes, across plain / current-thread / multi-thread
runtimes, mirroring the Phase-2 property test) is a Phase-5 exit criterion.

---

## 13. Updated phase plan (accounts for the WasmEngine)

The old "Phase 9 (Optional, Tier-2) Wasm engine" stub is **promoted and restructured**
into a first-class, multi-phase track:

| Phase | Work | ADR |
|---|---|---|
| **P5 (new)** | **WasmEngine core (Tier-2):** `wasmi` runtime, fuel metering, sandbox, frozen ABI, W1 pure-computation + W2 read-only host fns; determinism property test. `Wasm` opt-in. | ADR-018 |
| P6 | **On-chain deployment** (was P5): engine-agnostic — deploys Spec *and* Wasm modules; module validation; size cap. | ADR-015 |
| **P7 (new)** | **WasmEngine state & storage (W3):** per-contract storage, `sload`/`sstore`, storage delta in `result_data`, committer apply, storage gas + cap. Makes it "rich". | ADR-018 |
| P8 | **Upgrade governance** (was P6): owner registry, M-of-N, timelock — applies to Wasm modules (swap `asset_data`). | ADR-017 |
| P9 | **Model X cross-contract calls** (was P7): now the **Wasm `call` host import** (+ `SpecEngine` ISA `Call`); snapshot-ref validation, revert policy, gas on both chains. | ADR-016 |
| P10 | **E2E + determinism (incl. Wasm) + docs + vault closeout** (was P8). | — |

The WasmEngine **core** (P5) is testable off-chain and lands before deployment (P6),
mirroring how the `SpecEngine` landed in P2 before deployment. Its **stateful**
capabilities (W3) depend on deployment (P6) and land in P7; its **cross-contract**
capability (W4) lands with Model X in P9.

---

## 14. Open design decisions (approve to lock ADR-018)

- **QW1 — Runtime:** `wasmi` (interpreter) vs `wasm3` (JIT). **Recommend `wasmi`.**
- **QW2 — Storage writes:** emit-a-canonical-delta-in-`result_data` (recommended —
  consistent with ADR-013, keeps the sandbox pure) vs EVM-style host-write + accumulate.
- **QW3 — Default-on vs opt-in:** `Wasm` in `register_defaults` vs opt-in via the
  environment `contract_engines` spec. **Recommend opt-in** (existing chains don't
  suddenly accept WASM).
- **QW4 — Floating point:** allow `f32`/`f64` vs disallow in Tier-1. **Recommend
  disallow** (integer-only) to make determinism airtight.
- **QW5 — ABI style:** entrypoint + pointer/length with a standard `__alloc` (recommended)
  vs a fixed linear-memory buffer convention.
- **QW6 — Caps:** confirm the default caps in §8 (memory pages, module size, storage,
  call depth) as protocol-tunable constants.
