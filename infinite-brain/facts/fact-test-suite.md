---
id: fact-test-suite
title: "Test baseline 2026-10-01: 988 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/01/2026 workspace test baseline: 988 passed / 37 ignored / 0 failed — core 654 lib (+11 integration), committer 118 (109 lib + 9 integration), executor 33, finalizer 61, node-server 39, prover 15, sentinel 57."
auto_inject: false
applicable_when: "Checking test health, regressing after a change, or quoting baseline counts"
confidence: 1.0
verified_at: "10/01/2026"
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

# Test baseline 2026-10-01

`cargo test --workspace` at HEAD on 10/01/2026: **988 passed, 37 ignored, 0 failed**.

Per crate (lib + integration): pneumatic_core 654 lib / 19 ignored + 11 integration (pipeline 1 +
shielded_attacks 2 + shielded_pipeline 2 + transport 6 + rns_live 0/1-ignored); committer 118
(109 lib + 9 integration) / 7 ignored; executor 33; finalizer 61; node-server 39 / 2 ignored;
prover 15 / 2 ignored; sentinel 57 / 1 ignored.

Delta vs the 09/26 baseline (835 passed / 37 ignored): **+153 passed / 0 ignored** — the contract
execution plan (Phases 1–10, all landed 09/28→10/01) is the driver: core 548→654 (+106, the
`ContractEngine` substrate, Transfer/Spec/Wasm engines, W3 storage, deploy, upgrade governance,
ADR-016 cross-contract calls, and the `wasm_call_wasm_round_trip` re-pin), committer 101→118 (+17,
deploy/commit paths), executor 10→33 (+23, incl. the 2 cross-executor determinism tests), node-server
32→39 (+7, the 7 composite e2e pipeline tests). The P10 e2e also forced the rmp named-maps wire fix
(`fact-rmp-wire-named-maps`), which re-pinned `NON_SHIELDED_BASELINE` and regenerated the
`wasm_caller.wasm` fixture — count-neutral to the suite but byte-significant to the wire format.
