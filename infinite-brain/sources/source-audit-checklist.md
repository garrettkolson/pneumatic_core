---
id: source-audit-checklist
title: "Source: AUDIT_CHECKLIST.md (audit remediation)"
type: source
namespace: pneumatic
visibility: namespace
summary: "Remediation checklist for the 2026-08-19 audit (7C/16H/14M/8L; not production-ready). Phase-ordered items with Verify steps; ground rules inherited by the shielded S-phases; Phase 7.2–7.4 open."
auto_inject: false
applicable_when: "Checking the ground rules for new work, which audit items are done, or where the recorded test counts come from"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when AUDIT_CHECKLIST.md is edited"
tags: [audit, checklist, source, remediation, ground-rules]
edges:
  - target: pattern-fail-closed
    type: supports
    weight: 0.9
    note: "Ground rule 5 (lines 22-23) codifies fail-closed as a repo-wide discipline"
  - target: fact-test-suite
    type: related_to
    weight: 0.8
    note: "Records the test-count progression (659 at PQ phase, line 1351) the suite facts build on"
  - target: source-tasks-md
    type: related_to
    weight: 0.7
    note: "The later, phase-ordered successor work document"
related: []
source_url: "repo:AUDIT_CHECKLIST.md"
---

# Source: AUDIT_CHECKLIST.md

`AUDIT_CHECKLIST.md` (1460 lines) is the remediation checklist for the full production-readiness & security audit of 2026-08-19, whose verdict at audit time was **not production-ready** with 7 Critical / 16 High / 14 Medium / 8 Low findings (lines 1–5).

It defines the **ground rules** (lines 13–23) that every subsequent shielded S-phase carries into its work: (1) `cargo check` + full workspace test suite green after *every* item; (2) ≥1 regression test that fails without the fix; (3) some existing tests encode buggy behavior and must be updated; (4) no wire-shape change without a compatibility note; (5) fail closed, never silent-accept.

Structure: shielded phases S1 (line 25), S4.2 (165), S4.3 (242) plus audit Phases 1–8 (341–1381) and a composite node-server section (1148). Most boxes are checked; the **open items are Phase 7.2–7.4** (lines 1445–1453: cross-process determinism fixture, concurrency tests, boundary/adversarial tests). The overall **Done-when** (lines 1455–1460) additionally requires a clean multi-process run completing a transaction end-to-end over the real wire path.
