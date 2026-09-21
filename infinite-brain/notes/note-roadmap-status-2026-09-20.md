---
id: note-roadmap-status-2026-09-20
title: "Roadmap status 09/20/2026: S1.1–S5.2 landed; S5.3/S5.4/S6 remain"
type: note
namespace: pneumatic
visibility: namespace
summary: "Tier-1 shielded work at S5.2 (HEAD 1b7e4f0), atop landed PQ hybrid, RNS e2e, and the composite node-server runtime. Remaining: S5.3, S5.4, S6; Tier 2 (zk-VM) out of scope."
auto_inject: false
applicable_when: "Answering 'where are we on the roadmap' before planning the next shielded phase"
confidence: 0.95
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale once S5.3 lands (new HEAD) — supersede with a dated status note"
tags: [note, roadmap, status, shielded, progress]
edges:
  - target: event-s5-2-finalizer-wiring
    type: related_to
    weight: 1.0
    note: "Landed S5.2 = HEAD 1b7e4f0, the current frontier"
  - target: task-s5-3-pool-append
    type: related_to
    weight: 0.9
    note: "The immediate next step"
  - target: task-s5-4-real-pool-swap
    type: related_to
    weight: 0.9
    note: "The step after S5.3"
  - target: event-pq-hybrid
    type: related_to
    weight: 0.8
    note: "PQ hybrid migration (audit Phase 8) is landed"
  - target: event-rns-e2e
    type: related_to
    weight: 0.8
    note: "RNS transport e2e is landed"
  - target: event-node-server-composite
    type: related_to
    weight: 0.8
    note: "Composite runtime Phases 1–7 complete (README line 640)"
  - target: source-shielded-roadmap
    type: related_to
    weight: 0.9
    note: "Status is measured against this roadmap"
  - target: fact-test-suite
    type: related_to
    weight: 0.7
    note: "Live suite is 727 tests vs the 663 plan-writing snapshot (impl plan lines 1033-1037)"
related: []
source_url: "repo:plans/pneumatic-shielded-roadmap.md"
---

# Roadmap status — 09/20/2026

**Landed (shielded Tier 1):** S1.1–S5.2 are complete, with S5.2 (finalizer wiring) at HEAD `1b7e4f0`. Each S-phase entry in AUDIT_CHECKLIST.md is checked with its Done note. The shielded work sits on top of three other landed milestones: the **PQ hybrid** crypto migration (audit Phase 8; test count 658→659 per AUDIT_CHECKLIST.md line 1351), **RNS transport e2e**, and the **composite node-server runtime** (README "Phases 1–7 ✅ COMPLETE", line 640).

**Remaining (shielded Tier 1):** **S5.3** (committer pool append/persistence + nullifier commit) → **S5.4** (composite node-server real `Arc<ShieldedPool>` swap) → **S6.1–S6.4** (e2e, adversarial, concurrency, benchmarks; S6 done = all green, benchmarks recorded, open questions reported). Non-shielded open items: audit Phase 7.2–7.4 test items (AUDIT_CHECKLIST.md lines 1445–1453) and the TASKS.md "Remaining test gaps" tail (lines 912–914).

**Out of scope:** Tier 2 (hiding contract logic/state, zk-VM territory) is a hard boundary per the roadmap Part 0 and the impl plan (line 1010) — work expanding toward it must stop and report.

Test baseline: 663 at plan writing; **727 live** (impl plan lines 1033–1037, +64 since). Four roadmap open questions persist (external circuit audit, constrained-device proving UX, viewing-key policy, anonymity-set bootstrap).
