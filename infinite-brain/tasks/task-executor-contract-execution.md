---
id: task-executor-contract-execution
title: "Open: implement executor contract execution (plan Phases 1-10)"
type: task
namespace: pneumatic
visibility: namespace
summary: "Implement ADR-011-018 per plan Phases 1-10: P1-P9 landed (substrate+payload, Transfer/Spec engines, stub replacement, gas bounds, Tier-2 WasmEngine core, on-chain deploy, Wasm storage, upgrade governance, Model X cross-contract calls). Remaining: P10 e2e."
auto_inject: false
applicable_when: "Planning, scoping, or tracking the executor contract-execution implementation"
confidence: 0.9
verified_at: "09/30/2026"
verified_by: "dsh-agent"
staleness_signal: "Done when a composite e2e asserts a real result_hash and Phase 10 (e2e) is landed or re-scoped; P1-P9 landed (P9 09/30/2026)"
tags: [task, executor, contract-execution, implementation, adr-011, adr-014, adr-016]
edges:
  - target: decision-contract-engine-model
    type: depends_on
    weight: 0.9
    note: "Phases 1-2 implement the engine substrate and Tier-1 engines"
  - target: decision-tx-calldata-payload
    type: depends_on
    weight: 0.8
    note: "Phase 1 lands the payload wire field"
  - target: decision-executor-pure-read-only
    type: depends_on
    weight: 0.8
    note: "Phases 1 and 4 implement the delta model and gas bounds"
  - target: decision-contract-model-lifecycle
    type: depends_on
    weight: 0.85
    note: "Phases 6/8/9 implement deployment, governance, and Model X calls"
  - target: decision-wasm-engine-tier2
    type: depends_on
    weight: 0.9
    note: "Phases 5 (core) + 7 (storage) implement the Tier-2 WasmEngine"
  - target: decision-cross-contract-calls
    type: depends_on
    weight: 0.9
    note: "Phase 9 implements ADR-016 Model X cross-contract calls (Spec Op::Call + Wasm env.call)"
  - target: concept-executor-role
    type: part_of
    weight: 0.9
    note: "The work is the completion of the executor role's computation stage"
  - target: fact-executor-partition-key-defect
    type: related_to
    weight: 0.7
    note: "Phase 3 fixes the env_id partition-key defect"
  - target: fact-executor-backpressure-slot-leak
    type: related_to
    weight: 0.7
    note: "Phase 3 fixes the preload slot leak"
related: ["[[Open: executor contract execution is a stub (TODO at executor.rs:386)]]"]
source_url: "plans/executor-contract-execution-implementation-plan.md"
---

# Open: implement executor contract execution (plan Phases 1-10)

Implementation task for the approved contract-execution design (decisions locked
09/27-09/28/2026, ADR-011–014; **ADR-018 WasmEngine designed 09/28/2026**). Plan:
`plans/executor-contract-execution-implementation-plan.md`; WasmEngine design:
`plans/wasm-engine-design.md`; deploy design: `plans/deploy-contract-design.md`.
Status: **in progress** — Phases 1–9 complete (P1–P8 09/29/2026, **P9 09/30/2026**); the
Tier-2 WasmEngine **core** is landed (Phase 5), **on-chain deployment** is landed (Phase 6),
**Wasm storage (W3)** is landed (Phase 7), **upgrade governance** is landed (Phase 8), and
**Model X cross-contract calls** are landed (Phase 9, ADR-016). Remaining: P10 (e2e).

Phase checklist (each phase's exit criteria live in the plan):

- **P1 ✅ (09/28/2026)** — `src/contracts.rs` `ContractEngine` substrate in
  `pneumatic_core` + additive `payload`/`gas_limit` wire fields (ADR-011, ADR-012).
  Landed: `ContractEngine` trait, `ExecutionInput`/`ExecutionOutput`,
  `ContractError` → `ValidationFailureReason` mapping (new
  `ContractExecutionFailed` reason), `ContractEngineRegistry` (DashMap, ADR-002
  pattern) with fail-closed `Transfer`/`Spec` placeholders, `contract_engines`
  environment spec (default `["Transfer","Spec"]`, unknown name fails boot),
  `payload` + `gas_limit` on `Transaction` (skip-if-empty/zero, byte-identical
  legacy wire — regression-tested) joining `CanonicalTransaction` (sender signs
  calldata — regression-tested). Workspace `cargo test` green; core suite 558.
- **P2 ✅ (09/28/2026)** — `TransferEngine` + `SpecEngine` (versioned rmp ISA) +
  determinism property tests (ADR-011). Landed in `src/contracts.rs`:
  `TransferEngine` (validates amount, emits canonical rmp delta
  `(token_id, sender, receiver, amount, sequence_number)`; `TRANSFER_BASE_COST =
  21000`, `gas_limit == 0` = no cap); `SpecEngine` interpreter over
  `InstructionProgram { version: 1, ops }` — closed ISA `LoadTx(field)`,
  `LoadConst`, `Add`/`Sub`/`Mul`/`Mod` (checked — overflow/underflow/mod-zero
  revert), `Cmp` (1 >, 2 ==, 3 <), `Select`, `Emit` (pops stack top → 8-byte LE
  `result_data`; last emit wins), `Halt` (implicit at end of program); byte fields
  load as first ≤ 8 bytes, left-zero-padded, big-endian; gas = instruction count.
  Per-token selection: `select_engine()` — `contract_engine` metadata key, default
  `Transfer` for non-contract tokens, contract tokens without a key fail closed.
  Deviation from plan text: `Emit` pops the stack (plan wrote `Emit(value)`);
  an immediate-only emit would make the arithmetic ops unreachable. Determinism
  property test: 25 seeded random cases × both engines × 4 runs (plain ×2, tokio
  current-thread, tokio multi-thread) byte-identical. Core suite 558 → 572.
- **P3 ✅ (09/28/2026)** — stub replacement: real `execute_contract` dispatch,
  real `validate_execution_result`, and the D3/D4/D6 defect fixes. Landed in
  `executor/src/executor.rs`: `execute_contract` resolves the token's
  `SmartContract` asset (fail-closed `ContractNotFound`), selects the engine via
  `select_engine` (ADR-011), and runs it over `ExecutionInput` (the fetched
  sender state is the input — D6). `validate_execution_result` is now a pure
  module-scope free function (non-empty `result_data`/`result_hash`,
  `gas_used ≤ gas_limit` when capped, `TransferDelta` post-condition). **D3**:
  fetch under `token_partition_id` — new `partition_id` field threaded from
  `env_data.token_partition_id` in `node-server/.../plugins.rs`. **D4**:
  backpressure slots free on settle — `active_tasks` (slots, `HashSet`) is
  separated from `preload_tasks` (results, persist until `preload_cleanup`), so
  `Err` outcomes are recorded (`Result<ExecutionResult, String>`) and the slot
  is released in the spawned task; a contract-level failure transitions the tx
  to `Failed` with the engine's reasons. `ExecutionResult` carries `gas_used`;
  a token must carry a `SmartContract` asset (ADR-014). New tests: 4 dispatch
  tests (happy-path computed `result_hash` + Sign vote over it, failure→`Failed`,
  D4 slot-free, D3 partition key) + 5 `validate_execution_result` unit tests;
  `pipeline_integration` (full wire test) passes with the real dispatch. Core
  572, executor 17, workspace `cargo test` green.
- **P4 ✅ (09/28/2026)** — safety hardening (ADR-013 / Q3.2): no unbounded
  resource path. Landed in `executor/src/executor.rs` (+ `pneumatic_core::errors`):
  **wall-clock backstop** — `tokio::time::timeout` around `run_execution` in
  `execute_task` (default **5 s**, env-configurable via
  `PNEUMATIC_EXECUTOR_TIMEOUT_SECS`, whole seconds); on fire the tx fails with the
  new `ValidationFailureReason::ExecutionTimeout` and its backpressure slot is
  freed (no hang, no leaked slot). **Panic isolation** — the engine runs on a
  **blocking thread** (`tokio::task::spawn_blocking`, so a stuck engine can't block
  the async worker) under `std::panic::catch_unwind`; a panicking engine fails the
  tx with `ContractExecutionFailed` (no new core variant — the existing `Failed`
  transition handles it). New `ExecutorError::ExecutionTimeout` unit variant;
  `execution_timeout` field on `Executor`/`ExecutorHandle` + `with_execution_timeout`
  builder + `execution_timeout_from_env()`. New `safety` test module: 3 tests —
  sleeping engine → `ExecutionTimeout` + `Failed` (not a hang), panicking engine →
  `ContractExecutionFailed` + no crash, backpressure under timeout load
  (`max_in_flight = 1`, sequential timed-out preloads never hit `AtCapacity`).
  moka LRU deliberately skipped (a perf optimization, not a safety property —
  stale reads in consensus-critical deterministic execution are dangerous).
  Operational limits (max_in_flight, timeout, gas cap) documented in README.
  Core 572, executor 20, workspace `cargo test` green.
- **P5 ✅ (09/29/2026)** — **WasmEngine core (Tier-2)** (ADR-018). Landed in
  `pneumatic_core::contracts::wasm` (new `src/contracts/wasm.rs`; `mod wasm` +
  `pub use wasm::WasmEngine` in `contracts.rs`; re-exported, name `"Wasm"`,
  **opt-in** — NOT in `register_defaults`, so `select_engine` and the default engine
  set are unchanged). `wasmi` pinned to **`1.1.0`** with
  `default-features = false, features = ["std"]` — the `wat`/`wast`/`wasm-encoder` 2.x
  dependency chain (MSRV 1.88; we're on 1.87) is excluded because we consume binary
  WASM only, never WAT text. **Gas = WASM fuel** (ADR-013): `gas_limit` is the fuel
  budget (`0` ⇒ `u64::MAX` unbounded), an out-of-fuel trap
  (`TrapCode::OutOfFuel`) → `GasExhausted`, and `gas_used = budget − get_fuel()`.
  **Frozen ABI**: the module exports `__alloc(size)->ptr` +
  `execute(input_ptr,input_len,output_ptr,output_cap)->output_len`; the host allocates
  both buffers, writes the canonical `ExecutionInput` bytes in, and reads
  `result_data` from the output buffer (bounds-checked). **Sandbox**: only the `env`
  namespace is imported, and only the allow-listed W2 read-only host fns
  (`tx_amount`, `tx_sequence`, `sender_fuel`, `tx_payload_len`, `tx_payload`,
  `revert`); any other namespace or import → `BadBytecode` before instantiation.
  `revert` traps with a `HostError` marker (`downcast_ref`) → `Reverted`. **`f32`/
  `f64` disallowed** on the ABI boundary (export func/global signatures checked;
  imports are enforced by type-matching against the integer-only host fns). **Caps**
  (protocol-tunable): module ≤ 1 MiB, linear memory ≤ 32 MiB (post-call check; fuel
  metering is the primary bound on memory growth). Determinism: pure-Rust interpreter
  (no JIT), pinned runtime, state pinned to the execution-time snapshot (Model-X
  invariant). Six committed fixtures (`src/contracts/wasm_fixtures/*.wasm`, built by
  `compile.sh` with `--crate-type cdylib`): W1 sum (exact canonical output), W2
  `tx_amount`, revert, forbidden-import, f32-export, fuel-loop. 10 new tests incl.
  cross-executor determinism (plain / tokio current-thread / tokio multi-thread
  byte-identical). Core 572 → 582; workspace `cargo test` green.
- **P6 ✅ (09/29/2026)** — **on-chain deployment** (engine-agnostic, Spec + Wasm).
  Landed: `pneumatic_core::contracts::deploy` (`DeployParams`, `CreateTokenDelta`,
  `deploy_gas = 50_000 + 10·len`, deterministic CREATE2 `derive_token_id`,
  `deploy_contract`, `CreateTokenDelta::to_token` with committer metadata keys winning),
  `DeployValidationSpec` (fail-closed: parse → name 1–64 B → engine registered →
  Wasm ≤ 1 MiB + `validate_wasm_module` / Spec ≤ 64 KiB → nonce == sequence →
  `risk ≤ max_risk`), executor `run_execution` special-case for `action ==
  "DeployContract"`, and the committer `apply_deploy_delta` (re-derive delta as a pure
  fn of `(sender, nonce, DeployParams)`, `hash(delta) == tx.result_hash` integrity
  check else `TransactionPayloadMismatch`, idempotent `save_token`). **QD4 partition_id
  = environment_id** (operator override of the original token_id proposal). 8 deploy +
  10 validation + 3 committer + 1 executor tests. Core 582 → 601; workspace
  `cargo test` green.
- **P7 ✅ (09/29/2026)** — **WasmEngine state & storage (W3)** (ADR-018 / design §7).
  Landed in `pneumatic_core::contracts::wasm` + committer: `sload`/`sstore`/`sdelete`
  host imports (read-your-writes: delta first, then base) write a **storage delta** into
  the Wasm `result_data` envelope — `WasmResult { module_output, storage_delta }`,
  `StorageDelta = BTreeMap<Vec<u8>, Option<Vec<u8>>>` (`None` = tombstone; `BTreeMap` ⇒
  canonical sorted rmp order). **Transport (QD1):** new additive `Transaction::result_data:
  Vec<u8>` wire field (`#[serde(default, skip_if_empty)]`, NOT in `CanonicalTransaction`)
  — the delta is the module's `sstore` output and is **NOT re-derivable** from the tx
  (unlike the deploy delta), so the executor sets it post-exec and the committer consumes
  it. **Location (QD2):** `SmartContract::storage: BTreeMap<Vec<u8>, Vec<u8>>`
  (`#[serde(default)]`). **Gas + cap (QD3):** `sload` 100 / `sstore`-new 20000 /
  `sstore`-rewrite 5000 / `sdelete` 5000; post-apply total capped at 1 MiB (exceed →
  `Reverted`). Executor `execute_contract` now sets `tx.result_data`; new committer
  `apply_storage_delta` (dispatch on `action == "ContractCall"`) verifies
  `hash(result_data) == result_hash` (`TransactionPayloadMismatch`), decodes the
  `WasmResult`, and applies the delta to `SmartContract::storage` — Wasm-only (non-Wasm /
  missing token / empty `result_data` ⇒ no-op), idempotent (BTreeMap set/remove).
  `WASM_OUTPUT_CAP` reduced 64 KiB → 16 KiB (the module's 17-page memory leaves a small
  heap after the host-allocated output buffer; 64 KiB pushed the module's own `__alloc`
  out of bounds). 5 new Wasm tests (round-trip + canonical determinism + base-state read +
  gas + cap) + 7 committer apply tests. Core 582 → 628; committer 102+9; workspace
  `cargo test` green (cross-executor Wasm determinism re-verified).
- **P8 ✅ (09/29/2026)** — **upgrade governance** (ADR-017, design first). Landed in
  `pneumatic_core::contracts::upgrade` (new `src/contracts/upgrade.rs`: `UpgradeParams`,
  `ReplaceAssetDelta`, `upgrade_gas = 50_000 + 10·len`, `upgrade_digest`, `verify_quorum`,
  `timelock_satisfied`, `apply_replace_asset`), the `SmartContract` owner fields
  (`src/tokens.rs`: `owners: Vec<Vec<u8>>` + `threshold: u32`, both `#[serde(default)]`;
  `threshold == 0` ⇒ immutable), `pneumatic_core::validation::upgrade`
  (`UpgradeValidationSpec`: parse → contract → threshold>0 → quorum → bytecode cap +
  `validate_wasm_module` / Spec + scanner no-Reject → risk), the executor dispatch
  (`execute_upgrade`: re-validates the M-of-N quorum deterministically, emits the
  `ReplaceAssetDelta` in `result_data`), and the committer `apply_upgrade_delta`
  (dispatch on `action == "UpgradeContract"`: re-derive delta, `hash(delta) == result_hash`
  else `TransactionPayloadMismatch`, **threshold-0 no-op (defense in depth)**, timelock
  gate `epoch >= proposal_epoch + 1` else no-op, `apply_replace_asset`). **QD1** owner
  registry = dedicated `SmartContract` fields; **QD2** quorum = canonical digest
  `SHA256(b"PNEUMATIC/UPGRADE/v1" ‖ rmp(token_id, SHA256(bytecode), owners, threshold,
  epoch))` + Ed25519 M-of-N over the **current** owners (safe rotation); **QD3** timelock =
  apply-time gate, no pending store. Applies to Wasm modules (the delta swaps the module
  bytecode). 11 new core tests (7 `contracts::upgrade` + 4 `validation::upgrade`) + 3
  executor + 7 committer. Core 628 → 639; committer 102 → 109; executor 21 → 24;
  workspace `cargo test` green.
- **P9 ✅ (09/30/2026)** — **Model X cross-contract calls** (ADR-016; design first in
  `plans/model-x-call-design.md`, ADR-014 C4). Landed in `pneumatic_core::contracts::call`
  (new `src/contracts/call.rs`): `SnapshotRef { height, block_hash }` (0-based pin into B's
  chain), the `TargetStateProvider` trait (the executor's I/O surface — resolve
  `(target, ref, sender)` → `PinnedTarget`, **fail-closed** on any miss), `CallContext
  { provider, registry, depth }` riding a new `ExecutionInput.call_ctx: Option<Arc<..>>`
  (**excluded from `canonical_bytes()`** — same rule as `storage`, so no existing
  `result_hash` changes), `validate_snapshot_ref` (block exists at `height` AND
  `current_hash == ref.block_hash`), `synthesize_call_tx` (virtual B tx
  `id = "xcall/{A_tx_id}/{target_hex}"`, `action = entry_point`, `gas_limit = sub_budget`,
  `amount = None` — no cross-token value flow), and `execute_call` (resolve → select B's
  engine → run under the sub-budget `A_remaining − CALL_BASE` with `call_ctx = ctx.child()`;
  A charges `CALL_BASE + B.gas_used`; `MAX_CALL_DEPTH = 8`; a failed call charges
  `CALL_BASE` and is **data to A**, not a trap). Surfaces (Q5): `SpecEngine` gains
  `Op::Call { target_token, entry_point, call_payload, ref_height, ref_hash }` (pushes status
  1/0 on the stack); `WasmEngine` gains the W4 `env.call(target_ptr,len, entry_ptr,len,
  payload_ptr,len, ref_height, ref_hash_ptr,len, out_ptr,out_cap) -> i32` host import
  (result length, **0 on any failure — never traps**; `call_gas` folded into `gas_used`; in
  `ALLOWED_ENV_IMPORTS`). Gas constants `XCALL_CALL_BASE_SPEC = 10`, `XCALL_CALL_BASE_WASM =
  100`; commitment domain `b"PNEUMATIC/XCALL/COMMIT/v1"` (B-side
  `commitment = H(A_tx_id ‖ A_result_hash ‖ snapshot_ref)`, settled at commit on B's chain —
  **A's finality independent**, no two-phase atomic commit). Executor wiring
  (`executor/src/executor.rs`): private `ExecutorTargetProvider` (get_token →
  `validate_snapshot_ref` → `get_asset::<SmartContract>()` → get_user, all fail-closed to
  `InvalidInput`) + `call_ctx` built in `execute_contract`. **Wasm→Wasm round-trip** proven
  with a hand-assembled 578-byte `wasm_caller.wasm` fixture (see
  `fact-wasmparser-read-var-i32-bug` for the mandatory `sleb_force2` + no-data-section +
  `control.height` workarounds). 13 core call tests + 2 Wasm round-trip tests + 7 executor
  tests (6 provider fail-closed: unknown token, ref out of range, ref hash mismatch, no
  contract asset, missing user; + 1 full `execute_contract` wiring). Core 639 → 654;
  executor 24 → 31; workspace `cargo test` green (0 failed).
- **P10** — composite e2e (incl. a Wasm contract) + cross-executor determinism tests +
  docs + vault closeout.

Supersedes the narrower scope of `task-executor-contract-bytecode` (stub
replacement only), which stays open as the staleness marker until the code lands.
