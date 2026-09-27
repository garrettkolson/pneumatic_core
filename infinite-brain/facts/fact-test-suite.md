---
id: fact-test-suite
title: "Test baseline 2026-09-26: 835 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "09/26/2026 workspace test baseline: 835 passed / 37 ignored / 0 failed — core 548, committer 101 (92 lib + 9 integration), executor 10, finalizer 61, node-server 32, prover 15, sentinel 57, integration 19."
auto_inject: false
applicable_when: "Checking test health, regressing after a change, or quoting baseline counts"
confidence: 1.0
verified_at: "09/26/2026"
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
    note: "The ignored tests are the live-proving and live-RNS benchmark tests (gated by design — proving never runs in the default suite) plus a few gating doc-tests"
related: []
source_url: "Empty"
---

# Test baseline 2026-09-26

`cargo test --workspace` at HEAD on 09/26/2026: **835 passed, 37 ignored, 0 failed**.

Per crate: pneumatic_core 548 passed / 19 ignored (+1 doc-test); committer 101 passed / 7 ignored (92 lib + 9 integration); executor 10; finalizer 61; node-server 32 / 2 ignored; prover 15 / 2 ignored; sentinel 57 / 1 ignored; workspace integration 19 passed (committer 9 + transport 6 + pipeline 2 + shielded-pipeline 2); rns_live 0 / 1 ignored.

Delta vs the 09/23 baseline (828 passed / 32 ignored): **+7 passed / +5 ignored** — core 546→548 (+2) with integration/doc growth in the workspace-level suites. Historical: the 09/23 figure came after the modularization program (count-neutral moves) + 5 `auth::tests` cases; the 09/23 → 09/25 period added the S6.1–S6.4 fast tests and moved 16 long-running tests to `#[ignore]` (623c7f3); the 09/25 → 09/26 period landed the e2e-pipeline work (see log-organize-vault-20260726-e2e-closed and the 09/25 transport-layer log), which re-landed the suite at the 835/37 total.
