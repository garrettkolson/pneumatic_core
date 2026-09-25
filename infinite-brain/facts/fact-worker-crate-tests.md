---
id: fact-worker-crate-tests
title: "Per-worker-crate test counts verified 09/23/2026 (cargo test -p)"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Verified per-crate after S6: executor 10 passed, finalizer 61, committer 92 lib + 9 integration, sentinel 57 + 2 ignored, node-server 32 + 2 ignored, core 552 + 23 ignored, prover 15 + 2 ignored — 0 failed in every crate."
auto_inject: false
applicable_when: "Quoting per-crate test counts for the worker crates, checking test health after a change in executor/finalizer/committer/sentinel/node-server"
confidence: 1.0
verified_at: "09/25/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale whenever any worker crate's test modules change — re-run cargo test -p <crate> per crate"
tags: [tests, worker-crates, baseline, per-crate]
edges:
  - target: fact-test-suite
    type: derived_from
    weight: 0.9
    note: "Per-crate breakdown matching the workspace baseline (committer 101 = 92+9, sentinel 57, executor 10, finalizer 61, node-server 32)"
  - target: fact-workspace-layout
    type: related_to
    weight: 0.7
    note: "Counts are per workspace member of the 5-worker-crate + core + prover layout"
related: []
source_url: "Empty"
---

# Per-worker-crate test counts verified 09/25/2026 (post-S6)

`cargo test --workspace` on 09/25/2026 (all `0 failed`), after S6 (shielded Tier-1 completion, task-s6-shielded-completion): core grew from 546/19 to 552 passed / 23 ignored (S6.1 fast ×2 + S6.2 fast ×2 + S6.3 ×2 passed; S6.1 live ×2 + S6.2 live ×2 ignored) and prover from 15/1 to 15 passed / 2 ignored (S6.4 timing test, `#[ignore]`d); every other crate unchanged from the 09/23 baseline. Historical: the 09/23 figure came after the `EpochSnapshotCache<T>` extraction (sentinel −10, finalizer −5 moved into 7 core `epoch::tests::snapshot_cache_*`); S5.3/S5.4 grew the committer 80→101 (92 lib + 9 integration) and node-server 31→32 + 2 ignored; 623c7f3 moved 16 long-running tests to `#[ignore]` workspace-wide.

| Crate | Result |
|-------|--------|
| pneumatic_executor | 10 passed (lib) |
| pneumatic_finalizer | 61 passed (lib) |
| pneumatic_committer | 92 passed (lib) + 9 passed (`tests/pipeline_integration.rs`) = 101; 7 ignored |
| pneumatic_sentinel | 57 passed + 1 ignored (lib) + 1 ignored (doc-tests) |
| pneumatic_node_server | 32 passed + 2 ignored (lib) |

Notes on the ignored sentinel tests: the ignored doc-test is the one live end-to-end proof through the full sentinel path — `#[ignore = "live halo2 prove (~1 min); see the test's doc comment"]` at `sentinel/src/sentinel/tests/shielded.rs:613` (post-09/23 modularization; the S5.1 live e2e fixture that `pneumatic_sentinel`'s dev-dependencies on `pneumatic_prover`/`pasta_curves`/`ff`/`group` exist for, per `sentinel/Cargo.toml` dev-deps comment).

The committer's 9 integration tests live in `committer/tests/pipeline_integration.rs` — end-to-end pipeline scenarios over real RNS wire transports: no-conflict commit, conflict → resolution → slashing, commit-from-empty-registry materialization, and unregistered-sender rejection.

These per-crate numbers reconcile exactly with the workspace baseline recorded in fact-test-suite.
