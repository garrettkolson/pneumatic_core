---
id: fact-test-suite-s6
title: "Test baseline 2026-07-26: 835 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "07/26/2026 workspace test baseline after the e2e pipeline test landed + finalizer/executor test-module compile fixes: 835 passed / 37 ignored / 0 failed — core 553/19 (552 lib + 1 e2e pipeline_integration), committer 101/7, prover 15/2, rest unchanged. USE `cargo test --workspace` — plain `cargo test` at this root runs ONLY the root pneumatic_core package."
auto_inject: false
applicable_when: "Checking test health, regressing after a change, or quoting baseline counts"
confidence: 1.0
verified_at: "07/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale whenever the workspace test counts change — re-run cargo test --workspace"
tags: [tests, baseline, ci, quality, workspace-flag]
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

# Test baseline 2026-07-26 (post-e2e)

`cargo test --workspace` at HEAD on 07/26/2026: **835 passed, 37 ignored, 0 failed**.

Delta vs the 09/25 S6 baseline (834/37/0): **+1 passed** — the new `tests/pipeline_integration.rs` e2e (sentinel → executor → finalizer → committer, ≥2 nodes/role, all hops over real RNS). Core is now 553 passed / 19 ignored (552 lib + 1 e2e integration); every other crate unchanged — committer 101 / 7; prover 15 / 2; finalizer 61; sentinel 57 / 2; node-server 32 / 2; executor 10; transport_integration 6; rns_live 0 / 1. 834 + 1 = 835 ✓.

**⚠️ Workflow gotcha (found 07/26):** this is a workspace **with a root package** (`pneumatic_core`). Plain `cargo test` at the root runs **only the root `pneumatic_core` package** (~559 tests) — it does **not** compile or run the member crates (committer, executor, finalizer, sentinel, node-server, prover). You **must** use `cargo test --workspace` to build + run every crate's test modules. This is why the finalizer/executor test-module breakage (stale `signing.rs`, `make_test_keypair` missing `.expect()`, dead `make_test_signing_key`) went undetected until a full-workspace run.

Post-S6 deltas vs the 09/23 baseline (828/32/0): core +6 passed (S6.1 fast ×2, S6.2 fast ×2, S6.3 ×2) and +4 ignored (S6.1 live ×2, S6.2 live ×2); prover +1 ignored (S6.4 timing); everything else unchanged. 828 + 6 + 1 (e2e) = 835 ✓; 32 + 4 + 1 = 37 ✓.

All S6 live/benchmark tests are `#[ignore]`d with re-run commands in their `#[ignore = …]` attributes (the keygen gate: first check-4 touch per test binary pays the one-time ActionCircuit `keygen_vk`, ~2.5 min; a live prove is ~1 min release-equivalent work, much slower in the debug test profile — the single pipeline live test measured ~17.5 min in debug while sharing the box with two other cargo runs).
