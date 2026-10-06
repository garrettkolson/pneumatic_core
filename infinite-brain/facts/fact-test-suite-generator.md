---
id: fact-test-suite-generator
title: "Test baseline 2026-10-02 (testnet generator): 1084 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/02/2026 workspace baseline after testnet-gen and the config-path overrides: 1084 passed / 37 ignored / 0 failed (+19 over fact-test-suite-peer-derived-topology) — 17 in the new crate, 2 for the path-override rules."
auto_inject: false
applicable_when: "Comparing a test count after a change, or checking the regression baseline"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "SUPERSEDED 10/02/2026 by fact-test-suite-cloud (1101/37/0). Otherwise: any change that adds/removes tests."
tags: [fact, tests, baseline, testnet, generator, config]
edges:
  - target: fact-test-suite-peer-derived-topology
    type: related_to
    weight: 0.9
    note: "Supersedes the 1065/37/0 baseline"
  - target: fact-testnet-generator
    type: supports
    weight: 0.9
    note: "17 of the 19 new tests come from the generator crate"
related: []
source_url: "Empty"
---

# Test baseline 2026-10-02 (testnet generator): 1084 / 37 / 0

Measured 10/02/2026 with `cargo test --workspace`. **1084 passed / 37 ignored /
0 failed**, superseding 1065/37/0. Ignored unchanged at 37.

| Where | Count | What |
|---|---|---|
| `testnet-gen/src/topology.rs` | 6 | Full mesh vs role graph, the "only intra-executor links are droppable" property, non-overlapping port windows, **every edge resolves in both directions**, empty/single-node fail-closed, unresolvable peer is an error |
| `testnet-gen/src/topology.rs` (mode tests) | 2 | Explicit modes are answers at any size (pinned bug: `--topology full-mesh` was silently rewritten to role-graph above the threshold), and `auto` switches at its threshold |
| `testnet-gen/tests/generation.rs` | 8 | Emitted artifacts against the real consumers: `ConfigSpec`, `EnvironmentMetadata::load_from_spec`, `NodeIdentity::load_or_create` reload stability, both-direction j-rule port cross-check, genesis keyed by Ed25519 and never RNS, re-run preserves keys, `--force-keys` replaces, role graph reaches the emitted peer lists |
| `testnet-gen/tests/boot_config.rs` | 1 | A generated node through the real `Config::build()` — config, env dir, keystore — asserting the loaded identity is the one genesis was keyed by |
| `src/config.rs` | 2 | Path-override defaults unchanged; blank override treated as unset |

The j-rule test has been mutation-verified: reversing the emitter's direction makes
it fail with `sentinel-1 must forward to sentinel-2 on base 21009 + j 0`.
