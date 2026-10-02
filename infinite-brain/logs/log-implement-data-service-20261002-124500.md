---
id: log-implement-data-service-20261002-124500
type: log
operation: implement-code
date: "2026-10-02T12:45:00"
namespace: pneumatic
summary: "Built the missing data-service half (new crate data-service / pneumatic_data_service binary: framed MsgPack store speaking DataOp, byte-opaque store with write-through persistence, genesis seeder applied through the client API) plus tests/data_service_boot.rs driving the committer's real ShieldedPool::load against it; found and fixed a real core defect where get_user/save_user were private inherent methods so the trait impl inherited a default that discards with_source/with_secret; suite 1029 → 1047/37/0"
affected_nodes: ["fact-data-service", "fact-data-user-trait-default-defect", "fact-test-suite-testnet", "task-testnet-launcher", "fact-workspace-layout", "repo:docs/OPERATOR_RUNBOOK.md", "repo:data-service/"]
tags: ["log", "implement-code", "data-service", "genesis", "testnet", "trait-defaults", "defect-fix"]
---

Orientation was vault-first (INDEX → data-provider/rns/node-registry nodes), then
code verification where the vault predates the shielded/composite era. The user
asked how to spin up a testnet of decent size; scoping answers came back as
20-40 validators on one host full mesh, processes for dev plus generated compose
for promotion, and "data service + genesis first" for build order.

Two subagents were dispatched (multi-node harness analysis; RNS topology
constraints) and both returned claims that failed verification, which is recorded
in `task-testnet-launcher` because they would have shaped the design wrongly:
(1) `max_peers: 25` and `interface_count: 6` do not exist anywhere in rns-net
0.7.0 (crate-wide grep returned nothing; `with_transport_enabled` sets only
`transport_enabled`, `config_builder.rs:94-97`); (2) `NodeRegistryConfig::
register_direct_links` at registry.rs:131 does not exist anywhere in the repo.
First-hand verification did confirm the load-bearing facts the design rests on:
the port window `udp_port + 0 ..= udp_port + P+1`, the j-rule forward-port
target, leaf-vs-relay gating, the 48 h destination TTL, and the ~481 B direct
packet cap with Resources requiring live links.

Design decisions taken: the store is byte-opaque rather than envelope-aware
(integrity is the client's job; a service that recomputes hashes is a second
implementation of a consensus-critical fingerprint); genesis is applied through
`DefaultDataProvider` rather than hand-building envelopes, for the same reason;
absence is not modelled as a value, so a miss returns an empty body and the
client fails closed rather than trusting an invented record; the service is std
threads, not tokio, so a misbehaving node cannot stall the accept loop.

The significant find was not the testnet tooling at all. A probe test printed
`get_source() = Remote(127.0.0.1:port)` while `save_user` still failed against
the per-UID UDS path. Root cause: `get_user`/`save_user` were private *inherent*
methods on `DefaultDataProvider`, so `impl DataProvider for DefaultDataProvider`
inherited the trait default that constructs a fresh `DefaultDataProvider::new()`
and discards both `with_source` and `with_secret`. From inside `src/data.rs` the
inherent method is visible, so core's own wire tests exercised the correct path
and 1029 tests never caught it; from outside, every production caller holds
`Arc<dyn DataProvider>` and got the broken path. Field impact: with
`PNEUMATIC_DATA_ADDR` set to a remote service — the topology the runbook
documents — role selection and the registration stake gate read user rows from
the wrong endpoint, resolving stake 0, i.e. nodes that boot cleanly, install no
roles, and have every peer registration rejected. Fixed by implementing both on
the trait impl and deleting the inherent duplicates, with a regression guard that
holds the provider as `Arc<dyn DataProvider>` and asserts the write lands in the
service it was pointed at. The same trap remains live for
get/save token/data — recorded as an open follow-up in the defect fact rather
than fixed here, because making those methods required has workspace-wide blast
radius across ~12 `impl DataProvider` sites and deserves its own pass.

Also corrected the operator runbook, which stated the data service "is not part
of this repo" — now false — and added §5.1 documenting the four load-bearing
genesis records, the Ed25519-vs-RNS key trap, and the equal-stakes caveat.
`deploy/config/testnet/genesis.example.json` ships as a template guarded by
`tests/deploy_examples.rs`, which asserts cross-file agreement with the env spec.
