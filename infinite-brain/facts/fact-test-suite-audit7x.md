---
id: fact-test-suite-audit7x
title: "Test baseline 2026-10-01 (audit Phase 7 close): 1029 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Workspace baseline after closing AUDIT_CHECKLIST Phase 7 (7.2/7.3/7.4): 1029/37/0 (+8: 4 determinism-fixture, 2 committer reconcile-then-advance, 1 quorum-exactly-100, 1 unsigned-heartbeat); core lib 691, committer lib 111."
auto_inject: false
applicable_when: "Comparing test counts after this date, or checking the pre-existing failure baseline"
confidence: 0.95
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Any workspace test run; check against `cargo test --workspace`"
tags: [fact, tests, baseline, audit]
edges:
  - target: fact-test-suite-testgap-tail
    type: derived_from
    weight: 1.0
    note: "Supersedes the 1021-passed test-gap-tail baseline with the +8 audit Phase 7 delta"
  - target: source-audit-checklist
    type: related_to
    weight: 0.8
    note: "The 7.x items this run's tests close"
related: []
source_url: "repo:cargo test --workspace output, 10/01/2026"
---

# Test baseline 2026-10-01 (audit Phase 7 close): 1029 / 37 / 0

`cargo test --workspace` after closing the genuinely-open AUDIT_CHECKLIST Phase 7
items: **1029 passed / 37 ignored / 0 failed.**

Delta from `fact-test-suite-testgap-tail` (1021): **+8**:

| New tests | Where | Pins |
|---|---|---|
| 4 | `src/epoch/tests/determinism.rs` (7.2) | key-order-invariant leader/finalizer/shard/shuffle selection across domains×epochs×tips×salts; insertion-order-invariant canonical bytes + fingerprints; same-logical-block→identical `create_hash` with discriminating controls |
| 2 | `committer/src/committer/tests/epoch.rs` (7.3) | `handle_epoch_reconcile` advances exactly one epoch with detector mirroring, repeatable; snapshot-persist failure propagates fail-closed |
| 1 | `src/environment.rs` (7.4) | quorum range inclusive at 100.0 (rejections at 0.0 / >100 pre-existing) |
| 1 | `src/node/registry/tests/heartbeat.rs` (7.4) | literally-empty `binding_signature` rejected (forged-32-byte variant pre-existing) |

Core lib 685 → 691; committer lib 109 → 111. Ignored unchanged (37). With this, every
code-side item in `AUDIT_CHECKLIST.md` is checked; only the four S-close decision
gates remain open.
