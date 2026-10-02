---
id: fact-test-suite-peer-derived-topology
title: "Test baseline 2026-10-02 (peer-derived topology): 1065 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/02/2026 workspace baseline after tests/peered_topology_e2e.rs: 1065 passed / 37 ignored / 0 failed (+1 over fact-test-suite-peering). The added test is the suite's only multi-node test whose directories are derived by peering rather than seeded by the test."
auto_inject: false
applicable_when: "Comparing a test count after a change, or checking the regression baseline"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "Any change that adds/removes tests; superseded by the next recorded baseline"
tags: [fact, tests, baseline, peering, integration-tests]
edges:
  - target: fact-test-suite-peering
    type: related_to
    weight: 0.9
    note: "Supersedes the 1064/37/0 baseline"
  - target: fact-integration-test-fixture-blind-spot
    type: supports
    weight: 0.9
    note: "The single added test is the fixture-blind-spot fix"
related: []
source_url: "Empty"
---

# Test baseline 2026-10-02 (peer-derived topology): 1065 / 37 / 0

Measured 10/02/2026 with `cargo test --workspace`. **1065 passed / 37 ignored /
0 failed**, superseding the 1064/37/0 baseline in `fact-test-suite-peering`.
Ignored unchanged at 37.

The +1 is `tests/peered_topology_e2e.rs::a_cluster_derived_by_peering_routes_the_
pipelines_real_hops`: four nodes (one per role) in a full UDP mesh that discover
each other through the registration protocol, then route the pipeline's real
hops over the directories they derived. 2.1 s, stable across four consecutive
runs. It is mutation-verified — reinstating the `RegisterAck` defect makes it
fail with the sentinel holding two entries in its own bucket.

Suite shape after this change, which matters more than the count: the root
integration tests (`pipeline_integration`, `shielded_pipeline`,
`transport_integration`) still seed directories with `register_peer`, a test-only
API, and remain blind to the registration layer — see
`fact-integration-test-fixture-blind-spot`. One test now covers that layer;
converting the others to derived topologies is an open item on
`task-testnet-launcher`, worth doing when the launcher's key/port matrix makes
larger meshes easy to express.
