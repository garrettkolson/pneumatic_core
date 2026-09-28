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
— Phase 1 complete (09/28/2026).

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
- **P2** — `TransferEngine` + `SpecEngine` (versioned rmp ISA) + determinism
  property tests (ADR-011).
- **P3** — stub replacement: dispatch by `contract_engine` metadata; fix the
  partition-key defect and the backpressure slot leak (facts above).
- **P4** — safety bounds: instruction budget vs `gas_limit`, wall-clock timeout,
  panic isolation, `GasExhausted` → `Failed` (ADR-013).
- **P5** — on-chain deployment: `DeployContract` action, deterministic token id,
  `CreateToken` delta, committer apply (ADR-015 design first).
- **P6** — upgrade governance: owner registry, M-of-N multisig, 1-epoch timelock
  (ADR-017 design first).
- **P7** — Model X cross-contract calls: snapshot-pinned `Call`, cross-referenced
  tx on B's chain, deterministic revert/compensation (ADR-016 design first).
- **P8** — composite e2e + cross-executor determinism tests + docs + vault closeout.

Supersedes the narrower scope of `task-executor-contract-bytecode` (stub
replacement only), which stays open as the staleness marker until the code lands.
