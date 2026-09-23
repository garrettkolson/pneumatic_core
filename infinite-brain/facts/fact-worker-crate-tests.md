---
id: fact-worker-crate-tests
title: "Per-worker-crate test counts verified 09/23/2026 (cargo test -p)"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Verified per-crate after the EpochSnapshotCache<T> extraction: executor 10 passed, finalizer 61, committer 92 lib + 9 integration, sentinel 57 + 2 ignored (live halo2 e2e + doc), node-server 32 + 2 ignored — 0 failed in every crate."
auto_inject: false
applicable_when: "Quoting per-crate test counts for the worker crates, checking test health after a change in executor/finalizer/committer/sentinel/node-server"
confidence: 1.0
verified_at: "09/23/2026"
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

# Per-worker-crate test counts verified 09/23/2026

`cargo test --workspace` on 09/23/2026 (all `0 failed`), after the `EpochSnapshotCache<T>` extraction (task-monolith-modularization step 1): sentinel −10 and finalizer −5 cache tests moved into 7 core `epoch::tests::snapshot_cache_*` tests; S5.3/S5.4 shielded work grew the committer from 80 to 101 (92 lib + 9 integration) and node-server from 31 to 32 + 2 ignored; 623c7f3 moved 16 long-running tests to `#[ignore]` workspace-wide.

| Crate | Result |
|-------|--------|
| pneumatic_executor | 10 passed (lib) |
| pneumatic_finalizer | 61 passed (lib) |
| pneumatic_committer | 92 passed (lib) + 9 passed (`tests/pipeline_integration.rs`) = 101; 7 ignored |
| pneumatic_sentinel | 57 passed + 1 ignored (lib) + 1 ignored (doc-tests) |
| pneumatic_node_server | 32 passed + 2 ignored (lib) |

Notes on the ignored sentinel tests: the ignored doc-test is the one live end-to-end proof through the full sentinel path — `#[ignore = "live halo2 prove (~1 min); see the test's doc comment"]` at `sentinel/src/sentinel.rs:3280` (the S5.1 live e2e fixture that `pneumatic_sentinel`'s dev-dependencies on `pneumatic_prover`/`pasta_curves`/`ff`/`group` exist for, per `sentinel/Cargo.toml` dev-deps comment).

The committer's 9 integration tests live in `committer/tests/pipeline_integration.rs` — end-to-end pipeline scenarios over real RNS wire transports: no-conflict commit, conflict → resolution → slashing, commit-from-empty-registry materialization, and unregistered-sender rejection.

These per-crate numbers reconcile exactly with the workspace baseline recorded in fact-test-suite.
