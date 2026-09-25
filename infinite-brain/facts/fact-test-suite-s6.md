---
id: fact-test-suite-s6
title: "Test baseline 2026-09-25: 834 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "09/25/2026 workspace test baseline after S6 (shielded Tier-1 completion): 834 passed / 37 ignored / 0 failed — core 552/23, committer 101/7, prover 15/2, rest unchanged from the 09/23 baseline."
auto_inject: false
applicable_when: "Checking test health, regressing after a change, or quoting baseline counts"
confidence: 1.0
verified_at: "09/25/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale whenever the workspace test counts change — re-run cargo test --workspace"
tags: [tests, baseline, ci, quality]
edges:
  - target: fact-test-suite
    type: precedes
    weight: 0.9
    note: "Supersedes the 09/23 828/32/0 baseline"
  - target: task-s6-shielded-completion
    type: derived_from
    weight: 0.9
    note: "The S6 tests this baseline reflects"
related: []
source_url: "Empty"
---

# Test baseline 2026-09-25 (post-S6)

`cargo test --workspace` at HEAD on 09/25/2026: **834 passed, 37 ignored, 0 failed**.

Per-crate deltas vs the 09/23 baseline (828/32/0): core +6 passed (S6.1 fast ×2, S6.2 fast ×2, S6.3 ×2) and +4 ignored (S6.1 live ×2, S6.2 live ×2); prover +1 ignored (S6.4 timing); everything else unchanged — core 552 passed / 23 ignored; committer 101 / 7; prover 15 / 2; finalizer 61; sentinel 57 / 2; node-server 32 / 2; executor 10; integration 6; rns_live 0 / 1. 828 + 6 = 834 ✓; 32 + 4 + 1 = 37 ✓.

All S6 live/benchmark tests are `#[ignore]`d with re-run commands in their `#[ignore = …]` attributes (the keygen gate: first check-4 touch per test binary pays the one-time ActionCircuit `keygen_vk`, ~2.5 min; a live prove is ~1 min release-equivalent work, much slower in the debug test profile — the single pipeline live test measured ~17.5 min in debug while sharing the box with two other cargo runs).
