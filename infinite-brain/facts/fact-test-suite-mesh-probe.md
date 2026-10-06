---
id: fact-test-suite-mesh-probe
title: "Test baseline 2026-10-05 (mesh probe): 1118 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/05/2026 workspace baseline after the mesh probe and manifest reload: 1118/37/0 (+17) — 10 probe unit tests, 2 manifest round-trip tests, 5 probe CLI tests."
auto_inject: false
applicable_when: "Comparing a test count after a change, or checking the regression baseline"
confidence: 1.0
verified_at: "10/05/2026"
verified_by: "dsh-agent"
staleness_signal: "SUPERSEDED 10/05/2026 by fact-test-suite-fragments (1132/37/0). Otherwise: any change that adds/removes tests."
tags: [fact, tests, baseline, probe, mesh, deployment]
edges:
  - target: fact-mesh-verification-probe
    type: supports
    weight: 0.9
    note: "All 15 probe tests come from this change"
  - target: fact-test-suite-cloud
    type: related_to
    weight: 0.9
    note: "Supersedes the 1101/37/0 baseline"
related: []
source_url: "Empty"
---

# Test baseline 2026-10-05 (mesh probe): 1118 / 37 / 0

Measured 10/05/2026 with `cargo test --workspace`. **1118 passed / 37 ignored /
0 failed**, superseding 1101/37/0. Ignored unchanged at 37.

| Where | Count | What |
|---|---|---|
| `testnet-gen/src/probe.rs` | 10 | Complete mesh verifies clean with exact edge counts; a dropped entry is named by observer/role/member; self-membership; unknown rhash; wrong-bucket (distinct from absence); **silent node is UNKNOWN not empty and fails the run**; role-graph sparsity is expected (non-vacuous, two executors, and strictly sparser than full mesh for the same counts); snapshot/manifest identity mismatch; a foreign node in the snapshot is refused not ignored; snapshot JSON round-trip |
| `testnet-gen/tests/generation.rs` | 2 | `manifest.json` rebuilds the exact emitted topology (names, base ports, addresses, peer sets, interface counts); a manifest naming an unknown peer **fails the reload** rather than silently dropping edges |
| `testnet-gen/tests/probe_cli.rs` | 5 | Real binaries end to end: healthy cluster exits 0 **and its success message carries the reachability caveat**; injected defects are classified and exit 1; a silent node exits 1 and prints PARTIAL; `--json` is machine-readable for CI; unreadable inputs exit **2**, distinct from a mesh fault (1) |

**Mutation-verified:** suppressing the `Missing` check in `evaluate` fails the unit
test *and* the CLI test independently — the judgment is covered at both layers, so
an always-green probe cannot be introduced without a failure.

The probe's own suite is deliberately layered: pure judgment (cheap, exhaustive),
manifest reload (fidelity of the expectation), and CLI (exit codes an operator and
CI actually depend on).
