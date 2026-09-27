---
id: hyp-readme-postmvp-phase-8-outstanding
title: "Hypothesis: README Phase 8 production-readiness work is planned, not done"
type: hypothesis
namespace: pneumatic
visibility: namespace
summary: "Phase 8 production readiness (rustdoc, ADR docs, operator runbook; LOW, Post-MVP) is documented as planned in TASKS.md; the other roadmap phases verify as done, so Phase 8 items are likely still outstanding."
auto_inject: false
applicable_when: "Assessing how much non-shielded production-readiness work remains before a release"
confidence: 0.5
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Resolve (delete/answer) when rustdoc/runbook work lands or TASKS.md marks Phase 8 complete"
tags: [hypothesis, roadmap, production-readiness, readme]
edges:
  - target: source-tasks-md
    type: related_to
    weight: 1.0
    note: "The Phase 8 claim now lives in TASKS.md (the README roadmap section was removed 09/26/2026)"
  - target: source-readme-adrs
    type: related_to
    weight: 0.5
    note: "Historical: the Phase 8 table was formerly in README lines 602-614, removed 09/26/2026"
  - target: source-tasks-md
    type: related_to
    weight: 0.6
    note: "TASKS.md's open test-gap tail corroborates outstanding post-MVP work"
  - target: source-audit-checklist
    type: related_to
    weight: 0.6
    note: "Audit Phase 7.2-7.4 test items outstanding in the same era"
related: []
source_url: "repo:README.md"
---

# Hypothesis: Phase 8 production readiness is outstanding

The roadmap's phase history (formerly in the README, now in TASKS.md and the vault's dated status notes) marks the composite node-server runtime **Phases 1–7 COMPLETE**, and the other phases verify against the repo: Phase 9's security-audit fixes are all FIXED/COMPLETE in TASKS.md, and Phase 10 (RNS transport) is landed.

**Phase 8: Production Readiness (Priority: LOW — Post-MVP)** — API documentation (rustdoc), architecture decision records, and an operator runbook — is *not* marked complete, and no evidence in the repo shows it was done: no runbook files were observed. (The ADR half of Phase 8 was effectively satisfied on 09/26/2026 when the ADRs were promoted to vault decision nodes; rustdoc and the runbook remain open.) Hence the hypothesis, at confidence 0.5: Phase 8 items remain outstanding planned work.

This matters for release scoping. Status questions should go to the vault's dated status notes (note-roadmap-status-2026-09-26 is current), not to any single file.
