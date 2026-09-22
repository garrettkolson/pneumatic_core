---
id: fact-test-suite
title: "Test baseline 2026-09-21: 823 passed / 9 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "09/21/2026 workspace test baseline after the committer `impl Committer` modularization: 823 passed / 9 ignored / 0 failed — committer 80 (71 lib + 9 integration, unchanged by the split), core 548 (+2 pre-existing growth vs the 09/20 546), executor 10, finalizer 66, node-server 31, prover 15, sentinel 67, integration 6."
auto_inject: false
applicable_when: "Checking test health, regressing after a change, or quoting baseline counts"
confidence: 1.0
verified_at: "09/21/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale whenever the workspace test counts change — re-run cargo test --workspace"
tags: [tests, baseline, ci, quality]
edges:
  - target: fact-workspace-layout
    type: derived_from
    weight: 0.8
    note: "Counts are per-crate in this layout"
  - target: pattern-cfg-test-proving
    type: supports
    weight: 0.9
    note: "The 9 ignored are the live-proving and live-RNS benchmark tests (gated by design — proving never runs in the default suite) plus a couple of gating doc-tests"
related: []
source_url: "Empty"
---

# Test baseline 2026-09-21

`cargo test --workspace` at HEAD on 09/21/2026: **823 passed, 9 ignored, 0 failed**.

Per crate: pneumatic_core 548 passed / 5 ignored; **committer 80 passed / 0 ignored** (71 lib + 9 integration) — unchanged by the Phase 2 `impl Committer` split into the five child modules under `committer/src/committer/`; executor 10; finalizer 66; node-server 31; prover 15 / 1 ignored; sentinel 67 / 1 ignored; transport_integration 6; rns_live 0 / 1 ignored; plus one crate with 0 passed / 1 ignored doc-test.

The delta vs the 09/20 figure (822 passed / 8 ignored) is pre-existing: `pneumatic_core` grew +2 unit tests (546 -> 548) and one doc-test is now counted — the committer crate (the only one Phase 2 touched) is byte-for-byte its 80/0 baseline, and the full suite is 0 failed.
