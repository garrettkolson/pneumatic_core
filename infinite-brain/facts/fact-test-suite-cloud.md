---
id: fact-test-suite-cloud
title: "Test baseline 2026-10-02 (multi-host prep): 1101 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/02/2026 workspace baseline after the listen-IP fix, loadable ip_address, and per-host placement: 1101/37/0 (+17). Notably the composite's bind change from 127.0.0.1 to all-interfaces did not disturb any e2e test, because binding 'any' is a superset of binding loopback."
auto_inject: false
applicable_when: "Comparing a test count after a change, or checking the regression baseline"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "SUPERSEDED 10/05/2026 by fact-test-suite-mesh-probe (1118/37/0). Otherwise: any change that adds/removes tests."
tags: [fact, tests, baseline, transport, testnet, deployment]
edges:
  - target: fact-test-suite-generator
    type: related_to
    weight: 0.9
    note: "Supersedes the 1084/37/0 baseline"
  - target: fact-transport-loopback-bind-default
    type: supports
    weight: 0.8
    note: "Records that the bind change was verified not to regress the suite"
related: []
source_url: "Empty"
---

# Test baseline 2026-10-02 (multi-host prep): 1101 / 37 / 0

Measured 10/02/2026 with `cargo test --workspace`. **1101 passed / 37 ignored /
0 failed**, superseding 1084/37/0. Ignored unchanged at 37.

| Where | Count | What |
|---|---|---|
| `src/config.rs` | 6 | Bind address defaults (unset/blank → unspecified), configured interface honored and whitespace-tolerant, **malformed address stops boot and quotes the value**, `rns_listen_ip` → `0.0.0.0` when unspecified and verbatim otherwise, `ip_address` optional in the spec schema |
| `testnet-gen/src/topology.rs` | 4 | Per-host placement shares one base port and assigns addresses in plan order; single-host keeps windows disjoint and dials loopback; address count must match exactly (blank entry rejected); the j-rule still resolves both ways on a shared range |
| `testnet-gen/tests/generation.rs` | 3 | Every bootstrap entry carries **its own peer's** address (56 directed edges checked; fails if the emitter writes a shared constant), one shared port range verified from the artifacts rather than the mesh, `ip_address` emitted only when configured |
| `testnet-gen/tests/cli.rs` | 4 | New file. Invokes the **binary** — flag parsing lives in `main.rs`, unreachable from the library: `--validators 10` makes exactly 10 nodes with a deterministic 3/3/2/2 split; a short address list fails naming both counts; a commented address file works and yields per-host placement; `--addresses` with `--addresses-file` is refused |

**The result worth remembering.** The composite's transport bind changed from
`127.0.0.1` to all interfaces (see `fact-transport-loopback-bind-default`) and
**nothing failed** — including the live-UDP e2e tests. Binding "any" is a superset
of binding loopback, so single-host suites cannot detect a loopback-only bind in
either direction. That is a general lesson about this suite's blindness, not a
property of this fix.
