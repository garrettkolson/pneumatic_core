---
id: note-roadmap-status-2026-10-01-phase8
title: "Roadmap status 10/01/2026 (late): Phase 8 production readiness landed; only test-gap tail + open questions remain"
type: note
namespace: pneumatic
visibility: namespace
summary: "Phase 8 (the last planned roadmap phase) is complete the same day as P10: telemetry/health/metrics, graceful shutdown in both binaries, the real composite node-server binary, Docker+compose deploy with guard-tested examples, the operator runbook, and the rustdoc pass. Remaining: the TASKS.md test-gap tail and the open question nodes."
auto_inject: false
applicable_when: "Answering 'where are we on the roadmap' after Phase 8"
confidence: 0.95
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the next work item lands (test-gap tail, audit, or S6.4) — supersede with a dated status note"
tags: [note, roadmap, status, phase-8, production-readiness]
edges:
  - target: note-roadmap-status-2026-10-01
    type: preceded_by
    weight: 1.0
    note: "Supersedes the earlier same-day snapshot (which listed Phase 8 as the open front)"
  - target: event-phase8-production-readiness
    type: related_to
    weight: 0.95
    note: "The milestone this snapshot records as landed"
  - target: task-data-provider-wire-tests
    type: related_to
    weight: 0.8
    note: "The TASKS.md test-gap tail is the main engineering work still open"
  - target: question-external-audit-before-real-value
    type: related_to
    weight: 0.7
    note: "Non-code open fronts: circuit audit, S6.4 proving UX, viewing-key compliance"
related: []
source_url: "Empty"
---

# Roadmap status — 10/01/2026 (post-Phase-8)

**Landed (this snapshot):** **Phase 8 — production readiness**, the last planned
roadmap phase, completed the same day as P10 closed the contract-execution plan.
See `event-phase8-production-readiness` for the itemized delivery
(`pneumatic_core::telemetry`; SIGINT/SIGTERM graceful shutdown in both binaries;
the real composite `node-server` binary replacing the scaffold stub; Dockerfile +
`deploy/docker-compose.yml` + guard-tested example configs; `docs/OPERATOR_RUNBOOK.md`;
crate/module rustdoc pass) and `concept-telemetry-health-metrics` for the durable
design.

**Landed (carried forward):** the whole legacy program (foundation, four worker
pipelines, optimistic finality, quorum gossip, RNS transport, SA_01–SA_08, hybrid
PQ crypto, composite node-server), shielded Tier-1 (S1.1–S6), and the executor
contract-execution plan (Phases 1–10).

**Remaining (nothing else is planned):**

1. **TASKS.md test-gap tail** — `DefaultDataProvider` wire-format tests
   (`task-data-provider-wire-tests`), server.rs async-poison, epoch stubs,
   registry `send_to_all`, config load/parse tests.
2. **Open questions** (product/audit, not roadmap tasks): external Action-circuit
   audit before real value, S6.4 proving-UX measurement, viewing-key compliance.
3. **Review flags** from the Phase 8 doc pass: `question-conns-review-flags`.

**Test baseline (verified 10/01/2026, post-Phase-8):** `cargo test --workspace` =
**997 passed / 37 ignored / 0 failed** (+9 over the P10 baseline: 7 telemetry + 2
deploy-example guard tests). Docker image `pneumatic:phase8` builds green.
