---
id: note-roadmap-status-2026-09-26
title: "Roadmap status 09/26/2026: shielded Tier-1 feature-complete; production readiness next"
type: note
namespace: pneumatic
visibility: namespace
summary: "S1.1–S6 all landed (S5.3 pool append, S5.4 real-pool swap 09/22, S6 close 09/25): shielded Tier-1 is feature-complete atop PQ hybrid, RNS e2e, and the composite runtime. Remaining: Phase 8 production readiness, the executor contract stub, and the TASKS.md test-gap tail."
auto_inject: false
applicable_when: "Answering 'where are we on the roadmap' after the shielded Tier-1 completion"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the next post-Tier-1 work item lands — supersede with a dated status note"
tags: [note, roadmap, status, shielded, production-readiness]
edges:
  - target: note-roadmap-status-2026-09-20
    type: preceded_by
    weight: 1.0
    note: "Supersedes the 09/20 snapshot (S5.2 frontier)"
  - target: note-roadmap-status-2026-10-01
    type: supersedes
    weight: 1.0
    note: "Superseded by the 10/01 snapshot (contract-execution plan complete)"
  - target: event-s6-shielded-completion
    type: related_to
    weight: 0.9
    note: "S6 close (09/25) is the newest landed milestone"
  - target: task-s5-4-real-pool-swap
    type: related_to
    weight: 0.8
    note: "S5.4 landed 09/22 — the real Arc<ShieldedPool> swap"
  - target: fact-test-suite
    type: related_to
    weight: 0.8
    note: "Current verified baseline for this status"
  - target: source-tasks-md
    type: related_to
    weight: 0.7
    note: "The remaining test-gap tail lives in TASKS.md"
  - target: hyp-readme-postmvp-phase-8-outstanding
    type: related_to
    weight: 0.8
    note: "Phase 8 production readiness is the open front"
related: []
source_url: "Empty"
---

# Roadmap status — 09/26/2026

**Landed (shielded Tier 1):** all of S1.1–S6. The last three: **S5.3** (committer pool append/persistence + nullifier commit), **S5.4** (composite node-server real `Arc<ShieldedPool>` wiring, 09/22), **S6** (attack suite, nullifier/Merkle concurrency, cross-crate 4-hop pipeline with wire-byte privacy assertion, prove/verify timing — closed 09/25). Tier-1 **private value transfer is feature-complete**.

**Landed (non-shielded):** the full legacy program — foundation, the four worker pipelines, optimistic finality + block gossip, deterministic per-tx routing, executor sharding, quorum gossip, RNS transport (Phase 10), security-audit fixes SA_01–SA_08, hybrid PQ crypto (Phase 7), and the composite node-server runtime (Phases 1–7).

**Remaining:** (1) **Phase 8 production readiness** — rustdoc, operator runbook, observability (metrics/tracing), deployment infra (Docker compose, health checks, graceful shutdown); (2) **executor contract execution** — `execute_contract` is still a documented stub (task-executor-contract-bytecode); (3) the **TASKS.md test-gap tail** (e.g. DefaultDataProvider wire-format tests — task-data-provider-wire-tests).

**Test baseline (verified 09/26/2026):** `cargo test --workspace` = **835 passed / 37 ignored / 0 failed** (core 548/19, committer 92+9/7, executor 10, finalizer 61, node-server 32/2, prover 15/2, sentinel 57/1, integration 19, rns_live 1 ignored, 1 doc-test).

**Documentation change (09/26/2026):** README.md trimmed from 690 to ~250 lines, contributor-focused. ADR-001…010 moved out of the README into first-class vault decision nodes (`decisions/decision-*.md`); the roadmap phase tables now live in TASKS.md + this note lineage.
