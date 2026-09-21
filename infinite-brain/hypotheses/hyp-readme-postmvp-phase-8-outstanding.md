---
id: hyp-readme-postmvp-phase-8-outstanding
title: "Hypothesis: README Phase 8 production-readiness work is planned, not done"
type: hypothesis
namespace: pneumatic
visibility: namespace
summary: "README's Phase 8 (rustdoc, ADR docs, operator runbook; LOW, Post-MVP) is documented as planned; the other roadmap phases verify as done, so Phase 8 items are likely still outstanding."
auto_inject: false
applicable_when: "Assessing how much non-shielded production-readiness work remains before a release"
confidence: 0.5
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Resolve (delete/answer) when rustdoc/runbook work lands or README marks Phase 8 complete"
tags: [hypothesis, roadmap, production-readiness, readme]
edges:
  - target: source-readme-adrs
    type: related_to
    weight: 1.0
    note: "The source of the Phase 8 claim (README lines 602-614)"
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

The README's "Roadmap to Production Deployment" marks the composite node-server runtime **Phases 1–7 ✅ COMPLETE** (line 640), and its other phases verify against the repo: Phase 9's security-audit fixes are all FIXED/COMPLETE in TASKS.md, and Phase 10 (RNS transport) is landed.

**Phase 8: Production Readiness (Priority: LOW — Post-MVP)** (lines 602–614) — API documentation (rustdoc), architecture decision records, and an operator runbook — is *not* marked complete, and I found no evidence in the repo that it was done: no runbook files were observed, and the ADRs exist only as the README section itself. Hence the hypothesis, at confidence 0.5: Phase 8 items remain outstanding planned work.

This matters for release scoping (and it's why the README is low-priority for *status* questions — it tracks the pre-shielded roadmap; the vault's dated status notes are the better source of truth).
