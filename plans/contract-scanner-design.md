# Contract Scanner Design — Deterministic deploy-time analysis + canary

**Status:** DESIGN-FOR-APPROVAL — extends ADR-015 (deployment).
**Date:** 2026-09-29.
**Depends on:** ADR-008 (determinism), ADR-011 (pluggable engines), ADR-013 (pure
executor / canonical deltas), ADR-015 (on-chain deployment — Phase 6 landed), ADR-018
(WasmEngine — P5 landed).
**Related plans:** `plans/deploy-contract-design.md`, `plans/wasm-engine-design.md`
(§7 W3/W4, §8 caps), `plans/executor-contract-execution-implementation-plan.md`
(P7, P9).

---

## 0. Summary

A **deterministic, pure, fail-closed contract scanner** runs at deploy validation
(inside `DeployValidationSpec`) to reject obvious malware / attack vectors *before* a
contract is deployed and starts costing gas. It has three engine-dispatched components:

1. **Spec well-formedness** — decidable static analysis of the closed, straight-line
   `Spec` AST.
2. **Wasm static walk** — heuristic opcode/section analysis of the WASM module via
   `wasmparser`.
3. **Wasm canary run** — a deterministic deploy-time smoke test: execute the module once
   against a **fixed canonical input** and a **fixed fuel budget**, then check the
   outcome.

Findings carry a **severity**: `Reject` (fails the deploy closed) vs `Warn` (flagged for
review). The scanner is **consensus-safe by construction** — every component is a pure
function of `(engine, bytecode)` plus protocol-frozen constants, so it yields an
identical verdict on every shard member (ADR-008). It is **defense-in-depth on top of
the sandbox**, not a replacement: the sandbox + fuel + caps are the guarantee that a
contract *cannot* do harm; the scanner is the cheap pre-screen that rejects obvious
garbage and raises the bar.

The plan also specifies the **P7 (W3 storage)** and **P9 (W4 / Model X cross-contract)**
extensions, where the attack surface grows and the scanner becomes essential.

---

## 1. Threat model — what a contract can and cannot do today

The scanner is scoped to the *actual* capability surface. A deployed contract
(engine-agnostic) can today:

- Read the 6 Wasm host inputs (`tx_amount`, `tx_sequence`, `sender_fuel`,
  `tx_payload_len`, `tx_payload`, `revert`) or the `Spec` `LoadTx` fields.
- Compute over its own linear memory (Wasm) / stack (Spec).
- Emit a `result_data` delta.

It **cannot** (this is what the sandbox + caps already guarantee, independent of the
scanner):

- Touch the network, FS, clock, or RNG (no such imports; the `env.*` allow-list is
  enforced at `wasm.rs:139`).
- Escape its linear memory or use non-allow-listed host fns.
- Run unbounded (fuel metering + the Phase-4 wall-clock backstop).
- Grow memory past 32 MiB (`WASM_MAX_MEMORY_BYTES`).
- Use `f32`/`f64` **on the ABI/export boundary** (`wasm.rs:129`) — *but not internally*
  (see §3.2: this is the gap the scanner closes).
- Read/write persistent state (W3 storage not landed) or call other contracts
  (W4 / Model X not landed).

**So the real, scanner-relevant attack vectors are:**

| Vector | Mechanism | Component that catches it |
|---|---|---|
| Economic DoS | contract burns its full gas budget on every call | canary run (fuel) + Wasm loop heuristic |
| Output / chain spam | contract emits a maximal `result_data` on a trivial input | canary run (output cap) |
| Non-determinism | internal `f32`/`f64` opcodes (evades the ABI-boundary check) | Wasm static walk (opcode scan) |
| Obfuscation | high-complexity code that is hard to audit | Wasm static walk (complexity metrics) |
| Embedded payload | large `data`/custom section used as a data channel | Wasm static walk (section sizes) |
| Broken / trivially-bad `Spec` | unknown `LoadTx` field, stack underflow, huge program | Spec well-formedness |

---

## 2. The hard constraint: consensus determinism (ADR-008)

This is the rule that shapes the entire design. The scanner runs in the **sentinel's
validation path**, which is consensus-critical: every shard member must compute the
**same verdict** for the same deploy transaction. Therefore the scanner must be:

- **Pure** — no I/O, no network, no clock, no RNG, no external service (no VirusTotal).
- **Deterministic** — a pure function of `(engine, bytecode)` + protocol-frozen constants.
- **Fail-closed** — any ambiguity → `Reject`, never a silent pass.
- **Bounded-cost** — a static walk over a ≤ 1 MiB module is microsecond-to-ms; the
  canary run is bounded by the canary fuel budget. Acceptable on the sentinel path.

This **rules out** probabilistic / ML classifiers and any non-deterministic heuristic.
The canary input **must be a fixed canonical constant** (not derived from live state),
or it would break determinism. (§3.3.)

---

## 3. The three components

`scan_contract(engine: &str, bytecode: &[u8]) -> Vec<ScanFinding>` dispatches by engine
name. New module `src/contracts/scan.rs`.

### 3.1 Spec well-formedness (decidable)

The `Spec` ISA (`contracts.rs:363`) is a **straight-line stack program**: `LoadTx`,
`LoadConst`, `Add`/`Sub`/`Mul`/`Mod` (all *checked*), `Cmp`, `Select`, `Emit`, `Halt`.
There are **no loops, no branches, no calls, no storage, no I/O** — so the classic
malware vectors do not exist, and the analysis is **decidable** (unlike Wasm).

Checks (all pure functions of the AST bytes):

1. **Parse** — the `bytecode` deserializes to an `InstructionProgram { version, ops }`;
   `version == 1`; otherwise `Reject` (`BadBytecode`).
2. **Known `LoadTx` fields** — every `LoadTx(f)` field name is in
   `{amount, sequence_number, gas_limit, sender, receiver, token_id, payload}`. An
   unknown field reverts at runtime (`load_tx_field`), so this is a *quality* pre-check
   → `Warn` (operators may tighten to `Reject`; see QDS2).
3. **Exact stack-depth tracking** — because the program is straight-line (no branches),
   the stack depth after each op is a **deterministic** value. Walk the ops maintaining
   the depth using each op's known pop/push counts; if any op would underflow, the
   program `Reverted`-s at runtime (safe, `contracts.rs:488`) → `Warn` (broken contract,
   not a security hole).
4. **Instruction / gas bound** — `ops.len() <= SPEC_MAX_INSTRUCTIONS` (each op = 1 gas,
   no loops, so max gas = `ops.len()`; this makes the `Spec` gas bound explicit and
   finite). Exceed → `Reject`.

Confidence: **high / near-decidable.** `Spec` is effectively malware-free by
construction; this check is a cheap formality plus a quality gate.

### 3.2 Wasm static walk (heuristic)

`wasmi::Module` exposes the module **header** (imports, exports, memories, custom
sections — `module/mod.rs:209`) but **not the decoded opcode bodies**. Opcode-level
analysis therefore needs a binary parser: **`wasmparser`** (already in `Cargo.lock` at
`0.239.0` as a transitive dep of `wasmi 1.1.0`; we add it as a **direct**, exact-pinned
dependency — no new transitive version is pulled). The `wasmparser::Parser` →
`CodeReader` stream yields concrete `Op` variants (`F32Add`, `MemoryGrow`, `Loop`, …)
for a full, deterministic walk.

Section-by-section (pure function of the module bytes):

| Check | Method | Severity |
|---|---|---|
| **Import surface** | re-affirm only `env.*` + the allow-list (`wasm.rs:57`); any other namespace/name | `Reject` (already enforced at `validate_wasm_module`; tripwire) |
| **f32/f64 opcodes** | scan every function body for any `F32*`/`F64*` op | **`Reject`** — closes the ABI-boundary gap (`wasm.rs:129` only checks exports); enforces the integer-only contract surface at the opcode level (a consensus-critical invariant) |
| **Export surface** | frozen ABI is `__alloc` + `execute`; flag any extra export | `Warn` (QDS3) |
| **Memory growth** | count `MemoryGrow`; flag above `WASM_MAX_MEMORY_GROW` | `Warn` |
| **Unbounded no-fuel loop** | track `loop`/`block` nesting; flag a `loop` back-edge whose body has no fuel-consuming instruction (call / memory op / `trap` / exiting `br`) | `Warn` (fuel already bounds it at runtime; pre-screen) |
| **Embedded data** | `data` section total bytes > `WASM_MAX_DATA_BYTES` | `Warn` (payload-staging vector) |
| **Custom sections** | total custom-section bytes > `WASM_MAX_CUSTOM_BYTES` | `Warn` (steganography vector) |
| **Obfuscation / complexity** | total functions, total opcodes, max loop nesting, control-flow edge count > thresholds | `Warn` (manual-review flag) |

Confidence: **heuristic** — Wasm is Turing-complete, so no static walk is exhaustive.
These checks catch the *obvious* vectors; they are tripwires, not a proof.

### 3.3 Wasm canary run (deterministic smoke test)

Execute the module **once** at deploy validation against a **fixed canonical input** and
a **fixed fuel budget**, then inspect the outcome. Because the input is a frozen constant
and the engine is a pure deterministic function, the result is **identical on every node**
(consensus-safe).

**The fixed canonical `ExecutionInput`** (a protocol-frozen constant, not live state):

- `tx` — a synthetic `Transaction`: all-zero `sender`, `amount = 0`,
  `sequence_number = 0`, `gas_limit = CANARY_FUEL_BUDGET`, `payload = b""`, fixed
  `token_id`.
- `contract` — the `SmartContract` being deployed (name, bytecode, version).
- `sender_state` — a fixed `User` (`fuel_balance = CANARY_FUEL_BUDGET`, `nonce = 0`,
  `stake = 0`).
- `token` — a synthetic contract `Token` (bytecode in `asset_data`).
- `gas_limit` — `CANARY_FUEL_BUDGET` (protocol-tunable; QDS4).

**Checks** (via `engine.execute(canary_input)`):

| Outcome | Disposition |
|---|---|
| `Err(GasExhausted)` | `Reject` — the contract burns its full budget on a trivial input → economic-DoS vector |
| `Err(GasExhausted)`-adjacent trap / `BadBytecode` / `InvalidInput` | `Reject` |
| `Ok { gas_used, result_data }` with `result_data.len() > CANARY_MAX_OUTPUT` | `Reject` — output-spam vector |
| `Ok { gas_used <= CANARY_FUEL_BUDGET, result_data.len() <= CANARY_MAX_OUTPUT }` | pass |
| `Err(Reverted(..))` | **accept** — reverting on the canary input is a legitimate contract outcome, not malice (a contract that always reverts is useless but not dangerous) |

**Limit:** the canary probes exactly **one** input. A contract can be clean on the canary
input and hostile on real inputs — so the canary is a smoke test, not a guarantee.

**P7/P9 note:** the canary must always use a **fixed, canonical snapshot**. Today that
snapshot is empty (no W3 storage, no W4 cross-contract targets). When those land, the
canary's snapshot extends to a fixed empty storage map (P7) and a fixed multi-contract
snapshot (P9) — see §9/§10.

---

## 4. The finding model

```rust
// src/contracts/scan.rs
pub enum Severity { Reject, Warn }

pub enum ScanCode {
    // Spec
    SpecBadBytecode, SpecUnknownLoadTx, SpecStackUnderflow, SpecTooManyInstructions,
    // Wasm static
    WasmBadImport, WasmFloatOpcode, WasmExtraExport, WasmMemoryGrowth,
    WasmUnboundedLoop, WasmLargeData, WasmLargeCustom, WasmObfuscated,
    // Canary
    CanaryGasExhausted, CanaryOutputOverflow,
}

pub struct ScanFinding {
    pub code: ScanCode,
    pub severity: Severity,
    pub detail: String,   // human/audit-readable; not part of the consensus hash
}

pub fn scan_contract(engine: &str, bytecode: &[u8]) -> Vec<ScanFinding>;
```

`detail` is audit/observability text; **the consensus-critical output is the set of
`(code, severity)` pairs**, which is a pure function of the inputs. The scanner is wired
into `DeployValidationSpec::validate` as a new step (after the existing module check at
`deploy.rs:80`):

```rust
let findings = scan_contract(&params.engine, &params.bytecode);
if findings.iter().any(|f| f.severity == Severity::Reject) {
    return Err(PneumaticError::Validation(vec![ValidationFailureReason::ContractScanFailed]));
}
// v1: Warn findings are logged (env_data.logger); see QDS5 for the metadata-attach option
```

A new `ValidationFailureReason::ContractScanFailed` variant is added (`errors.rs`).

---

## 5. Caps & tunables (protocol-tunable, frozen per chain)

| Constant | Purpose | Suggested default |
|---|---|---|
| `CANARY_FUEL_BUDGET` | canary-run fuel cap | 1_000_000 |
| `CANARY_MAX_OUTPUT` | canary `result_data` size cap | 4 KiB |
| `SPEC_MAX_INSTRUCTIONS` | `Spec` op count (explicit gas bound) | 1_000 |
| `WASM_MAX_MEMORY_GROW` | `MemoryGrow` op count threshold | 64 |
| `WASM_MAX_DATA_BYTES` | total `data` section size | 64 KiB |
| `WASM_MAX_CUSTOM_BYTES` | total custom-section size | 16 KiB |
| complexity thresholds | function/op/nesting/edge counts | per design, tuned off fixtures |

All are protocol constants (frozen per chain, like the existing `WASM_MAX_*` caps in
`wasm.rs:47`). Tuning them is a consensus event; the defaults above are starting points
to be validated against the `wasm_fixtures`.

---

## 6. Consensus-safety argument

- **Spec well-formedness** — pure parse + straight-line walk of the AST bytes. Deterministic.
- **Wasm static walk** — `wasmparser` is a deterministic binary parser; the walk is a
  pure function of the module bytes. Deterministic.
- **Canary run** — `engine.execute(fixed_input)` is a pure deterministic function of
  `(bytecode, fixed_input)`. Deterministic.

All three are pure functions of `(engine, bytecode)` + frozen constants, so every shard
member computes the same `Vec<(ScanCode, Severity)>`. **This is proven, not assumed, by
cross-executor determinism tests** (§13): run `scan_contract` on the same bytecode under
plain / tokio current-thread / tokio multi-thread and assert byte-identical findings —
the same pattern already used for the engines (ADR-008).

---

## 7. What changes in the codebase

- **New** `src/contracts/scan.rs` — the scanner (three components + finding model).
- **New** direct dependency `wasmparser = "=0.239.0"` (exact-pinned, security-sensitive,
  already in the lockfile via `wasmi`; no new transitive version).
- **Extend** `src/contracts.rs` — `pub mod scan;` + re-exports; new
  `SPEC_MAX_INSTRUCTIONS` constant.
- **Extend** `src/validation/deploy.rs` — `DeployValidationSpec::validate` gains the scan
  step (needs the `ContractEngineRegistry`, already held, to fetch the engine for the
  canary run).
- **Extend** `src/errors.rs` — `ValidationFailureReason::ContractScanFailed`.
- **Tests** — see §13.

No changes to the `ContractEngine` trait, `select_engine`, the executor dispatch, the
committer apply path, or the wire format. The scanner is a **validation-time** gate only.

---

## 8. Phasing & effort

| Phase | Scope | Trigger | Effort |
|---|---|---|---|
| **S0 — scanner foundation** | `scan.rs` (Spec well-formedness + Wasm static walk + canary run), `wasmparser` dep, `DeployValidationSpec` wiring, `ContractScanFailed`, tests incl. cross-executor determinism | now (post-Phase 6) | **~2–3 days** |
| **S1 — storage-aware scan** | W3 storage-delta checks (§9) | lands with **P7** | ~1 day |
| **S2 — cross-contract scan** | W4 / Model X checks (§10) | lands with **P9** | ~2 days (hardest; partly design-review) |

S0 is self-contained and shippable on its own. S1/S2 are *additive* — each is a new set
of `ScanCode` variants + checks that activate only once the corresponding capability
exists (they are no-ops / skipped until P7/P9 land).

---

## 9. P7 (W3 storage) scanner additions

When per-contract `sload`/`sstore` storage lands (ADR-018 §7, a new storage field on the
contract token + a storage delta in `result_data`), the scanner gains:

1. **Storage-delta well-formedness** — the storage delta (ordered `(key, value)` pairs +
   tombstones) parses and is schema-valid; keys ≤ `STORAGE_MAX_KEY_BYTES`, values ≤
   `STORAGE_MAX_VALUE_BYTES`. `Reject` on a malformed delta.
2. **Per-contract storage cap** — the canary run (now against a **fixed empty storage
   snapshot**) must not exceed `STORAGE_MAX_SIZE`; a canary that tries to bloat storage
   past the cap → `Reject` (state-DoS vector).
3. **Storage gas accounting** — `sstore` (new key > overwrite) costs more than `sload`;
   the canary budget accounts for storage ops, and the static walk can flag a module that
   emits an abnormal number of `sstore` calls.
4. **State-DoS heuristic** — a contract whose canary writes near-max storage on a trivial
   input is flagged (`Warn`) for review.

The canary's snapshot becomes a **fixed canonical empty storage map** (deterministic).

---

## 10. P9 (W4 / Model X cross-contract) scanner additions

When the cross-contract `call(target, entry, payload, snapshot_ref)` host import lands
(ADR-018 §7 / ADR-016), the attack surface grows the most and the scanner becomes
**genuinely important**. Additions:

1. **`call` nesting depth** — enforce the `call` depth cap (e.g., 8, `wasm-engine-design.md:214`)
   statically where targets are static; count nested call sites. `Reject` on a static
   overflow.
2. **Re-entrancy (the central P9 risk)** — Model X calls are **non-atomic** and
   **snapshot-pinned**, and ADR-013 applies state changes as **deltas at commit**. So
   during `A.call(B, …)`, A's in-flight state changes are *not yet applied*. A re-entrant
   call `B.call(A, …)` therefore resolves A's state from the **pinned pre-call snapshot**:
   a malicious B can re-enter A and make it act on **stale state** — the classic
   "withdraw before the balance is zeroed" / "take the lock twice" pattern. This is the
   single most important P9 risk, and it decomposes into:
   - **The re-entrant state view (a fundamental design decision — QDS7).** Two options:
     - *Optimistic-apply (re-entrancy-safe):* A's in-flight storage deltas are applied to a
       **working view** before the re-entrant call resolves A's state, so the re-entrant
       call sees A's *updated* state (an "entered" flag is visible → re-entrancy is
       blocked). Cost: a per-call-frame working-set apply; determinism is preserved (the
       working view is a pure function of the call tree).
     - *Snapshot-only (re-entrancy-vulnerable):* the re-entrant call always reads the
       pinned pre-call snapshot → re-entrancy is possible → **every** calling contract must
       use a re-entrancy guard ("entered" flag), and the scanner must flag `call`s that
       lack one.
   - **Static cycle detection.** Direct self-call (`A → A`) and static 2+-node cycles
     (`A → B → A`) over the call graph. Targets are often *dynamic* (computed at runtime),
     so this is a **heuristic** (`Warn`, manual review) — the hardest static check in the
     plan. A tight 2-node cycle is the common exploit shape and the highest-value catch.
   - **Re-entrancy-guard heuristic (design-review gate).** Flag contracts that issue a
     `call` and then read/modify state the callee could re-enter, without a visible guard.
     Not decidable statically in a Turing-complete language → a **design-review** trigger,
     not an auto-verdict.
   - **Deterministic revert / compensation across re-entrant frames.** Because Model X is
     non-atomic, a re-entrant failure must roll back **deterministically** across the whole
     call tree, including the re-entrant frame. The compensation must be a pure function of
     `(snapshot, call tree)` so every shard member computes the same rollback (ties into
     item 5 below).
3. **Snapshot-ref well-formedness** — a `call` resolves the target's state at a pinned
   `(height, hash)` snapshot ref; the scanner/canary verify the ref is well-formed and the
   resolution is deterministic. `Reject` on a malformed ref.
4. **Sub-gas budget accounting** — each `call` gets a sub-budget; the canary verifies the
   total sub-budget accounting is sound (no double-spend of fuel across the call tree).
5. **Revert / compensation (design-review gate)** — Model X is **non-atomic**; a
   partial failure needs deterministic revert/compensation. This is largely a **design
   review** item (hard to scan statically in a Turing-complete language) — the scanner
   flags contracts with `call`s for a manual compensation audit, and the canary (against a
   **fixed multi-contract snapshot**) exercises the happy + a forced-revert path.

The canary's snapshot extends to a **fixed canonical multi-contract snapshot** (the target
contracts + their pinned state). The P9 canary adds a **re-entrancy probe**: a fixed
canary target contract that calls *back* into the caller, so the canary deterministically
verifies the caller (a) reverts safely on re-entrancy, and (b) does **not** double-spend
(balance / lock) on the re-entrant path. This is a pure function of
`(bytecode, fixed snapshot)`, so it is consensus-safe. Honest caveat: for P9 the scanner
is a **mix of static heuristics + a deterministic canary + a design-review gate**, because
re-entrancy and compensation correctness are not fully decidable statically.

---

## 11. Decisions to lock (operator sign-off)

| # | Decision | Recommendation |
|---|---|---|
| **QDS1** | f32/f64 **opcode-level** check → `Reject` (close the ABI-boundary gap) | **Yes** — it's a consensus-critical invariant; the ABI check alone is insufficient |
| **QDS2** | `Spec` unknown `LoadTx` field / stack underflow → `Reject` or `Warn` | `Warn` for v1 (both revert *safely* at runtime); operators may tighten to `Reject` |
| **QDS3** | Non-ABI extra Wasm exports → `Reject` or `Warn` | `Warn` for v1 (unusual, not provably harmful) |
| **QDS4** | `CANARY_FUEL_BUDGET` value | 1_000_000 (protocol-tunable) |
| **QDS5** | `Warn` surfacing: log-only vs attach to token `metadata` | **log-only for v1** (no wire change); metadata-attach later (requires the committer to re-run the pure scan, or a `notes` field on the validation result) |
| **QDS6** | `wasmparser` as a direct, exact-pinned dep (`=0.239.0`) | **Yes** — already in the lockfile via `wasmi`; no new transitive version; security-sensitive so exact-pinned like `wasmi` |
| **QDS7** | P9 **re-entrant state view**: optimistic-apply (re-entrancy-safe, per-frame working set) vs snapshot-only (requires a per-contract re-entrancy guard) | **Optimistic-apply** — makes re-entrancy safe by default; the snapshot-only option pushes the guard burden onto every contract and is harder to audit. Lands with P9, not S0 |

---

## 12. Honest limits

- **Turing-complete Wasm ⇒ no complete "not malware" guarantee** (halting problem). The
  scanner catches *obvious* vectors and flags *suspicious* modules; it is not a proof.
- **The sandbox is the wall; the scanner is the tripwire.** A contract is confined by the
  sandbox + fuel + caps regardless of whether it is "malicious." The scanner's value is
  cheap pre-screening, raising the bar, and flagging for review.
- **The canary probes one input** — it is a smoke test, not input-exhaustive.
- **Obfuscation is an arms race** — a determined actor can evade heuristics. The
  complexity/obfuscation checks are review triggers, not verdicts.
- **P9 is the hard case** — reentrancy/compensation are not statically decidable; that
  phase leans on design review + the deterministic canary.

---

## 13. Test plan

**Unit (per `ScanCode`, fixture-driven):**
- `Spec`: clean program (no findings); unknown `LoadTx` field (`Warn`); stack underflow
  (`Warn`); over-long program (`Reject`).
- `Wasm` static: module with an `f32` op (`Reject`); non-allow-listed import (`Reject`);
  extra export (`Warn`); large `data` section (`Warn`); unbounded no-fuel loop (`Warn`);
  clean module (no findings). Reuse/extend `src/contracts/wasm_fixtures`.
- Canary: module that burns fuel (`Reject` via `GasExhausted`); module that emits a huge
  output (`Reject`); module that reverts on the canary input (**accepted**); clean module
  (no findings).

**Consensus-safety (the critical ones, ADR-008):**
- **Cross-executor determinism**: run `scan_contract` on a battery of fixtures under
  plain / tokio current-thread / tokio multi-thread; assert byte-identical
  `Vec<(ScanCode, Severity)>`. (Same pattern as the engine determinism tests.)

**Integration:**
- Deploy a malicious contract (e.g. `f32` op, or fuel-burner) → **rejected** at
  `DeployValidationSpec`; deploy a clean contract → **accepted** (extends the Phase-6
  committer/executor deploy tests).

**Exit criteria:** all of the above green; `cargo test --workspace` green; the scanner
adds no non-determinism (proven by the cross-executor tests); no wire-format change.
