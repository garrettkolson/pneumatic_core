---
id: fact-test-suite
title: "Test baseline 2026-09-23: 823 passed / 32 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "09/23/2026 workspace test baseline after the EpochSnapshotCache<T> extraction (15 worker cache tests removed, 7 core tests added) and 623c7f3's long-running-test ignores: 823 passed / 32 ignored / 0 failed — core 541, committer 101 (92 lib + 9 integration), executor 10, finalizer 61, node-server 32, prover 15, sentinel 57, integration 6."
auto_inject: false
applicable_when: "Checking test health, regressing after a change, or quoting baseline counts"
confidence: 1.0
verified_at: "09/23/2026"
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

# Test baseline 2026-09-23

`cargo test --workspace` at HEAD on 09/23/2026: **823 passed, 32 ignored, 0 failed**.

Per crate: pneumatic_core 541 passed / 19 ignored; committer 101 passed / 7 ignored (92 lib + 9 integration); executor 10; finalizer 61; node-server 32 / 2 ignored; prover 15 / 1 ignored; sentinel 57 / 1 ignored; transport_integration 6; rns_live 0 / 1 ignored; plus one crate with 0 passed / 1 ignored doc-test.

Delta vs the 09/22 figure (847 passed / 16 ignored): **−24 passed / +16 ignored**. Two independent causes: (1) commit 623c7f3 ("ignored long-running tests w/ notes", 09/23 09:16) moved 16 long-running/live tests to `#[ignore]` (+16 ignored, −16 passed); (2) the `EpochSnapshotCache<T>` extraction deleted 15 worker-crate cache tests (sentinel 10, finalizer 5) and added 7 core tests (`epoch::tests::snapshot_cache_*`) (net −8 passed). 847 − 16 − 8 = 823 ✓; 16 + 16 = 32 ✓.
