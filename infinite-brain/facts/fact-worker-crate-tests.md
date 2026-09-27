---
id: fact-worker-crate-tests
title: "Per-worker-crate test counts verified 09/26/2026 (cargo test --workspace)"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Verified per-crate 09/26/2026: core 548 + 19 ignored, committer 92 lib + 9 integration (+7 ignored), executor 10, finalizer 61, node-server 32 + 2 ignored, prover 15 + 2 ignored, sentinel 57 + 1 ignored — 0 failed in every crate."
auto_inject: false
applicable_when: "Quoting per-crate test counts for the worker crates, checking test health after a change in executor/finalizer/committer/sentinel/node-server"
confidence: 1.0
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale whenever any worker crate's test modules change — re-run cargo test -p <crate> per crate"
tags: [tests, worker-crates, baseline, per-crate]
edges:
  - target: fact-test-suite
    type: derived_from
    weight: 0.9
    note: "Per-crate breakdown matching the 09/26 workspace baseline (committer 101 = 92+9, sentinel 57, executor 10, finalizer 61, node-server 32)"
  - target: fact-workspace-layout
    type: related_to
    weight: 0.7
    note: "Counts are per workspace member of the 5-worker-crate + core + prover layout"
related: []
source_url: "Empty"
---

# Per-worker-crate test counts verified 09/26/2026

`cargo test --workspace` on 09/26/2026 (all `0 failed`):

| Crate | Result |
|-------|--------|
| pneumatic_core | 548 passed + 19 ignored (lib) + 1 passed (doc-test) |
| pneumatic_committer | 92 passed (lib) + 9 passed (`tests/pipeline_integration.rs`) = 101; 7 ignored |
| pneumatic_executor | 10 passed (lib) |
| pneumatic_finalizer | 61 passed (lib) |
| pneumatic_node_server | 32 passed + 2 ignored (lib) |
| pneumatic_prover | 15 passed + 2 ignored (lib) |
| pneumatic_sentinel | 57 passed + 1 ignored (lib) |

Workspace integration (`tests/`): 19 passed total (transport 6, pipeline 2, shielded-pipeline 2, plus the committer's 9); `rns_live` 0 / 1 ignored.

Delta vs the 09/25 figures: core 552→548 passed / 23→19 ignored and sentinel 2→1 ignored — the 09/26 test-module fix (log-organize-vault-20260726-testmodule-fix) and e2e-pipeline work (log-organize-vault-20260726-e2e-closed) reorganized the core/sentinel test modules without changing the workspace total class; the full suite re-landed at 835/37 (fact-test-suite).

Notes on the ignored tests: the sentinel's ignored test is the live end-to-end proof through the full sentinel path — `#[ignore = "live halo2 prove (~1 min); see the test's doc comment"]` at `sentinel/src/sentinel/tests/shielded.rs:613` (post-09/23 modularization). The committer's 9 integration tests live in `committer/tests/pipeline_integration.rs` — end-to-end pipeline scenarios over real RNS wire transports: no-conflict commit, conflict → resolution → slashing, commit-from-empty-registry materialization, and unregistered-sender rejection. The prover's 2 ignored tests include the S6.4 prove/verify timing benchmark.

These per-crate numbers reconcile exactly with the workspace baseline recorded in fact-test-suite.
