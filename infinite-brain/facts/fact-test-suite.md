---
id: fact-test-suite
title: "Test baseline 2026-09-20: 822 passed / 8 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "09/20/2026 workspace test baseline: 822 passed / 8 ignored / 0 failed — core 546, committer 80, executor 10, finalizer 66, node-server 31, prover 15, sentinel 67, integration 6."
auto_inject: false
applicable_when: "Checking test health, regressing after a change, or quoting baseline counts"
confidence: 1.0
verified_at: "09/20/2026"
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
    note: "The 8 ignored are exactly the live-prove/RNS-live tests"
related: []
source_url: "Empty"
---

# Test baseline 2026-09-20

`cargo test --workspace` at HEAD (1b7e4f0) on 09/20/2026: **822 passed, 8 ignored, 0 failed**.

Per crate: pneumatic_core 546 passed / 5 ignored; committer 71 unit + 9 integration; executor 10; finalizer 66; node-server 31; prover 15 / 1 ignored; sentinel 67 / 1 ignored; transport_integration 6; rns_live 0 / 1 ignored.

The 8 ignored tests are the live-proving and live-RNS benchmark tests (gated by design — see the client-side-proving decision).
