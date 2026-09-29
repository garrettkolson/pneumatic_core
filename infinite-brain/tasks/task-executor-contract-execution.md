---
id: task-executor-contract-execution
title: "Open: implement executor contract execution (plan Phases 1-8)"
type: task
namespace: pneumatic
visibility: namespace
summary: "Implement ADR-011-014 per plan Phases 1-8: engine substrate + payload field, Transfer/Spec engines, stub replacement + D3/D4 fixes, gas bounds, on-chain deploy, upgrade governance, Model X calls, e2e."
auto_inject: false
applicable_when: "Planning, scoping, or tracking the executor contract-execution implementation"
confidence: 0.9
verified_at: "09/28/2026"
verified_by: "dsh-agent"
staleness_signal: "Done when execute_contract dispatches to a registered ContractEngine, a composite e2e asserts a real result_hash, and Phases 5-7 (deploy/governance/Model X) are landed or re-scoped"
tags: [task, executor, contract-execution, implementation, adr-011, adr-014]
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
    note: "Phases 5-7 implement deployment, governance, and Model X calls"
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

# Open: implement executor contract execution (plan Phases 1-8)

Implementation task for the approved contract-execution design (decisions locked
09/27-09/28/2026, ADR-011–014). Plan:
`plans/executor-contract-execution-implementation-plan.md`. Status: **in progress**
— Phase 4 complete (09/28/2026).

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
- **P5** — on-chain deployment: `DeployContract` action, deterministic token id,
  `CreateToken` delta, committer apply (ADR-015 design first).
- **P6** — upgrade governance: owner registry, M-of-N multisig, 1-epoch timelock
  (ADR-017 design first).
- **P7** — Model X cross-contract calls: snapshot-pinned `Call`, cross-referenced
  tx on B's chain, deterministic revert/compensation (ADR-016 design first).
- **P8** — composite e2e + cross-executor determinism tests + docs + vault closeout.

Supersedes the narrower scope of `task-executor-contract-bytecode` (stub
replacement only), which stays open as the staleness marker until the code lands.
