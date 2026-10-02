---
id: fact-test-suite-peering
title: "Test baseline 2026-10-02 (peering): 1064 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/02/2026 workspace baseline after the peering initiator: 1064 passed / 37 ignored / 0 failed (+17 over fact-test-suite-testnet) — 16 control-plane unit tests and the first two-node-over-UDP peering end-to-end test."
auto_inject: false
applicable_when: "Comparing a test count after a change, or checking the regression baseline"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "SUPERSEDED 10/02/2026 by fact-test-suite-peer-derived-topology (1065/37/0). Otherwise: any change that adds/removes tests"
tags: [fact, tests, baseline, peering, control-plane]
edges:
  - target: fact-test-suite-testnet
    type: related_to
    weight: 0.9
    note: "Supersedes the 1047/37/0 data-service baseline"
  - target: fact-control-plane-peering
    type: supports
    weight: 0.9
    note: "All 17 added tests come from the peering work"
related: []
source_url: "Empty"
---

# Test baseline 2026-10-02 (peering): 1064 passed / 37 ignored / 0 failed

Measured 10/02/2026 with `cargo test --workspace`. **1064 passed / 37 ignored /
0 failed**, superseding the 1047/37/0 baseline in `fact-test-suite-testnet`. The
ignored count is unchanged at 37 — nothing running was moved to `#[ignore]`.

The +17:

| Where | Count | What |
|---|---|---|
| `src/node/registry/tests/peering.rs` | 16 | Builders and bucket math (declared roles, multi-bucket placement, tampered role set rejected, empty-role fail-closed), the ack placement rule and its degenerate fallback, ack-learned peers being vouchable, heartbeat refresh, peer dedup across buckets, both frame shapes (bare request = black hole, control frame = delivered), the measured control-frame-vs-direct-cap invariant, bootstrap-peer rhash derivation with malformed-key skip, no-transport fail-closed, and the two directory gates (stranger not answered; registered peer answered with only vouchable entries) |
| `tests/peering_e2e.rs` | 1 | Two real `RnsNetwork` nodes on UDP peer with each other and land in each other's correct role directories; 0.8 s, stable across repeated runs |

Root-crate lib tests went 691 → 707; every other crate's count is unchanged, so
the registration and ack changes caused no regressions in the 1029 tests that
exercised them before.

`tests/peering_e2e.rs` binds UDP ports 4710/4721 and is **not** `#[ignore]`d —
like `tests/pipeline_integration.rs` it runs in the normal suite and completes in
under a second. It is the first test in the repo to move a control-plane message
between two registries over a socket.
