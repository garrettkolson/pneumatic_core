---
id: note-roadmap-status-2026-10-01
title: "Roadmap status 10/01/2026: executor contract execution complete (Phases 1-10); production readiness next"
type: note
namespace: pneumatic
visibility: namespace
summary: "The executor contract-execution plan is fully landed (Phases 1-10, 09/28→10/01): real Transfer/Spec/Wasm engines, W3 storage, deploy, upgrade governance, Model X cross-contract calls, plus P10 e2e + determinism. The rmp wire format moved to named maps (deploy fix). Remaining: Phase 8 production readiness + the TASKS.md test-gap tail."
auto_inject: false
applicable_when: "Answering 'where are we on the roadmap' after the contract-execution plan completion"
confidence: 0.95
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the next post-contract-execution work item lands — supersede with a dated status note"
tags: [note, roadmap, status, executor, contract-execution, production-readiness]
edges:
  - target: note-roadmap-status-2026-09-26
    type: preceded_by
    weight: 1.0
    note: "Supersedes the 09/26 snapshot (shielded Tier-1 feature-complete)"
  - target: event-p10-e2e-determinism
    type: related_to
    weight: 0.9
    note: "P10 close (10/01) is the newest landed milestone"
  - target: task-executor-contract-execution
    type: related_to
    weight: 0.9
    note: "The plan this snapshot marks complete (all 10 phases)"
  - target: fact-rmp-wire-named-maps
    type: related_to
    weight: 0.8
    note: "The deploy e2e forced the rmp named-maps wire format migration"
  - target: fact-test-suite
    type: related_to
    weight: 0.8
    note: "The 988/0/37 baseline is the post-P10 state for this snapshot"
  - target: hyp-readme-postmvp-phase-8-outstanding
    type: related_to
    weight: 0.8
    note: "Phase 8 production readiness remains the open front"
related: []
source_url: "Empty"
---

# Roadmap status — 10/01/2026

**Landed (this snapshot):** the **executor contract-execution plan is fully complete (Phases 1–10,
09/28 → 10/01/2026)**. The executor no longer runs a stub — `execute_contract` dispatches to a
pluggable `ContractEngine` registry: Tier-1 `Transfer` + `Spec` engines, the Tier-2 `WasmEngine`
(wasmi, W3 storage), on-chain deploy, upgrade governance, and ADR-016 Model X cross-contract calls.
Phase 10 closed the plan with **7 composite `node-server` e2e tests** (each asserting the committed
block's `result_hash` == an independent `hash(engine_output)`) and **2 cross-executor determinism
tests**. The `execute_contract` stub marker (`task-executor-contract-bytecode`) is closed.

**Landed (earlier, carried forward):** the full legacy program (foundation, four worker pipelines,
optimistic finality + block gossip, deterministic per-tx routing, executor sharding, quorum gossip,
RNS transport Phase 10, security-audit SA_01–SA_08, hybrid PQ crypto Phase 7, composite node-server
Phases 1–7) and shielded Tier-1 (S1.1–S6, feature-complete).

**Protocol change (10/01):** the rmp wire format moved from positional arrays to **named maps**
(`rmp_serde::to_vec_named`) to fix the deploy `TypeMismatch(Array16)` at commit — see
`fact-rmp-wire-named-maps`. Reads stay backward-compatible (the deserializer accepts both), but the
block-hash canonical bytes changed; `NON_SHIELDED_BASELINE` and the `wasm_caller.wasm` fixture were
re-pinned accordingly.

**Remaining:** (1) **Phase 8 production readiness** — rustdoc, operator runbook, observability
(metrics/tracing), deployment infra (Docker compose, health checks, graceful shutdown); (2) the
**TASKS.md test-gap tail** (e.g. `DefaultDataProvider` wire-format tests — `task-data-provider-wire-tests`).

**Test baseline (verified 10/01/2026):** `cargo test --workspace` = **988 passed / 37 ignored / 0
failed** (core 654 lib + 11 integration, committer 118, finalizer 61, sentinel 57, executor 33,
node-server 39, prover 15). Up from the 09/26 baseline of 835 — the +153 is the contract-execution
plan.
