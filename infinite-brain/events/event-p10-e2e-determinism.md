---
id: event-p10-e2e-determinism
title: "P10: contract-execution plan closed — composite e2e + cross-executor determinism (10/01/2026)"
type: event
namespace: pneumatic
visibility: namespace
summary: "2026-10-01: Phase 10 closes the executor contract-execution plan — 7 node-server pipeline e2e tests (transfer/Spec/Wasm/W3/deploy-Spec/deploy-Wasm/cross-contract) each assert committed result_hash == hash(engine_output), plus 2 cross-executor determinism tests. Deploy e2e forced the rmp named-maps wire fix; workspace 988/0/37."
auto_inject: false
applicable_when: "Dating the contract-execution plan's completion, auditing the e2e/determinism invariants, or picking up the production-readiness (Phase 8) open items"
confidence: 1.0
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Historical event — never goes stale"
tags: [event, executor, contract-execution, e2e, determinism, milestone, completion]
edges:
  - target: task-executor-contract-execution
    type: related_to
    weight: 1.0
    note: "The plan this event closes (all 10 phases)"
  - target: fact-rmp-wire-named-maps
    type: related_to
    weight: 0.9
    note: "The deploy e2e surfaced the rmp named-maps wire fix, landed as part of this phase"
  - target: fact-test-suite
    type: supports
    weight: 0.8
    note: "The 988/0/37 baseline is the post-P10 state"
  - target: concept-executor-role
    type: supports
    weight: 0.8
    note: "Confirms the executor's computation stage is real (engines + e2e-verified result_hash)"
  - target: event-s6-shielded-completion
    type: preceded_by
    weight: 0.7
    note: "The prior milestone in the vault's event lineage"
related: []
source_url: "plans/executor-contract-execution-implementation-plan.md"
---

# P10: contract-execution plan closed (10/01/2026)

Phase 10 — the final phase of the executor contract-execution plan
(`plans/executor-contract-execution-implementation-plan.md`) — closed 10/01/2026, landing the
end-to-end verification, the cross-executor determinism tests, the docs, and the vault closeout.

**Composite e2e (7 tests, `node-server/src/node_server/tests/e2e.rs`).** The full pipeline
`Sentinel Verify → Executor Preload → Finalizer Sign → Committer Commit` runs for (a) a standard
transfer, (b) a `SpecEngine` contract, (c) a `WasmEngine` contract, (d) a stateful W3 storage
contract, (e) a Spec deploy, (f) a Wasm deploy, and (g) a cross-contract call. Each test asserts the
committed block's `result_hash` equals an independently computed `hash(engine_output)` — the terminal
P10 invariant that the committed hash is the real engine output, not a stub.

**Cross-executor determinism (2 tests, `executor/src/executor/tests/determinism.rs`).** Two
independent `Executor` instances over identical shard inputs produce an **identical `result_hash`**
and two "Sign" votes. The key finding recorded in the test: the "Sign" vote's hybrid
Ed25519·ML-DSA-44 signature is **not** byte-reproducible (ML-DSA-44 uses a fresh random nonce per
signing), so determinism is asserted as identical `result_hash` + both signatures verifying under the
identity key — **not** byte-equal signatures. (The `crypto.rs:553` comment calling the ML-DSA half
"deterministic" is misleading and was flagged.)

**Deploy wire fix + collateral.** Landing the deploy e2e exposed the rmp `TypeMismatch(Array16)` at
commit: `serialize_to_bytes_rmp` moved from `to_vec` (positional arrays) to `to_vec_named` (named
maps) — see `fact-rmp-wire-named-maps`. Its one-time collateral: re-pinned `NON_SHIELDED_BASELINE`
(68→380 bytes) and regenerated `wasm_caller.wasm`'s baked-in genesis ref hash.

**Result.** Full workspace `cargo test` green: **988 passed / 0 failed / 37 ignored** (up from the
09/26 baseline of 835). TASKS.md executor tail + README Outstanding updated; the vault (this event,
`fact-rmp-wire-named-maps`, the two task nodes, `concept-executor-role`, `fact-test-suite`, and a new
roadmap-status note) synced to closeout.
