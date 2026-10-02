---
id: fact-test-suite-testnet
title: "Test baseline 2026-10-02 (data service): 1047 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/02/2026 workspace baseline after the data-service crate and the get_user trait-default fix: 1047 passed / 37 ignored / 0 failed (+18 over fact-test-suite-audit7x) — 14 data-service wire tests, 3 committer boot-gate tests, 1 genesis-example guard."
auto_inject: false
applicable_when: "Comparing a test count after a change, or checking the regression baseline"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "Any change that adds/removes tests; superseded by the next recorded baseline"
tags: [fact, tests, baseline, data-service, testnet]
edges:
  - target: fact-test-suite-audit7x
    type: related_to
    weight: 0.9
    note: "Supersedes the 1029/37/0 audit-Phase-7-close baseline"
  - target: fact-data-service
    type: supports
    weight: 0.9
    note: "All 18 added tests come from the data-service crate and its boot-gate/example guards"
  - target: fact-workspace-layout
    type: related_to
    weight: 0.6
    note: "Counts are per-crate; data-service is a new crate in this layout"
related: []
source_url: "Empty"
---

# Test baseline 2026-10-02 (data service): 1047 passed / 37 ignored / 0 failed

Measured 10/02/2026 with `cargo test --workspace` (plain `cargo test` runs only
the root crate). **1047 passed / 37 ignored / 0 failed**, superseding the
1029/37/0 baseline recorded in `fact-test-suite-audit7x`. The ignored count is
unchanged at 37 — no previously-running test was moved to `#[ignore]`.

The +18 breakdown:

| Where | Count | What |
|---|---|---|
| `data-service/tests/service_roundtrip.rs` | 14 | Real `DefaultDataProvider` over a real socket: user/stake/pool/token/data round-trips, envelope stored verbatim and re-verifying, per-epoch isolation, absent-key fail-closed, HMAC mismatch rejection, state-file restart, genesis application, two genesis validation rejects, and the `get_user` trait-dispatch regression guard |
| `tests/data_service_boot.rs` (root) | 3 | The committer's own `ShieldedPool::load` boot path: un-genesis-ed store fails with "refusing to re-seed", genesis-seeded store boots, second boot loads the persisted pool |
| `tests/deploy_examples.rs` (root) | +1 | `deploy/config/testnet/genesis.example.json` must parse as `GenesisSpec` and agree with `deploy/config/env/env.json` |

Root-crate lib tests stayed at 691 and committer at 111 — the core change
(`DataRequest` accessors, the `get_user`/`save_user` trait-impl fix) added no new
unit tests of its own in core; its guard lives in the data-service suite, where
the real client/socket is available.

Still `#[ignore]`d and still benchmark-only (never enable in CI): the live
proving and live-RNS tests that pay the one-time `keygen_vk` (~2.5 min) or bind
real UDP.
