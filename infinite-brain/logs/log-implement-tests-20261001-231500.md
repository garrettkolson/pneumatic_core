---
id: log-implement-tests-20261001-231500
type: log
operation: implement-tests
date: "2026-10-01T23:15:00"
namespace: pneumatic
summary: "Closed the TASKS.md 'Remaining test gaps' tail: +24 tests (9 data wire-format vs an in-process fake data service, 10 config load/parse via new path-injected seams, 3 epoch stub contracts, 2 ThreadPool async-worker semantics); send_to_all confirmed already covered by Phase 6.2 fanout tests; suite 997 → 1021/37/0"
affected_nodes: ["task-data-provider-wire-tests", "fact-test-suite-testgap-tail", "fact-test-suite-phase8", "concept-conn-abstraction", "_system/INDEX.md", "repo:TASKS.md"]
tags: ["log", "implement-tests", "data-provider", "wire-format", "config", "threadpool", "epoch-stubs"]
---

Closed the last TASKS.md test-gap section end to end. (1) `data.rs`: new
`wire_format_tests` module — an in-process fake data service on a temp UDS
path speaking the real channel protocol (`[4B BE len][HMAC-SHA256 tag(32) ||
rmp body]`, zero tag verifying vacuously without a secret). 9 tests cover the
provider's own serialize→frame→send→receive→deserialize path: request shape
(key/op/partition) as actually received, User/Get-Data round-trips, Save
payload framing, stake-snapshot and shielded-pool ENVELOPE round-trips across
a real socket plus tampered-hash rejection (`SnapshotCorrupt` — the S5.3
no-re-seed boot contract), garbage → `DeserializationError`, matched-secret
HMAC round-trip with service-side tag verification asserted, wrong-key
response → `PeerUnauthenticated`, connect-refusal fail-closed, and the
in-module get/save op-guard branches. (2) `server.rs`: the async-poison test
deferred since the C# port is replaced — `tokio::sync::Mutex` cannot poison,
so the true analogue (a panicking job unwinds the async worker loop; a later
job provably never runs) is tested, with a guard test pinning the non-poison
assumption. (3) `epoch/tests/stubs.rs`: 3 contract tests for the stub
reconciler/staking manager incl. dyn object safety. (4) `config.rs`:
behavior-identical split `load_spec_from(path)` /
`get_environment_metadata_from(dir)` out of the hardcoded
`config.json`/`/env` reads, then 10 tests incl. all four boot-failure classes
and the H6 out-of-range-quorum rejection. (5) `send_to_all`: gap list was
stale — Phase 6.2 `fanout.rs` already covers bounded success/timeout,
failure recording, self-inclusion; no new code. Verification:
`cargo test --workspace` → 1021 passed / 37 ignored / 0 failed (997+24,
ignored unchanged). Marked `task-data-provider-wire-tests` RESOLVED, new
fact `fact-test-suite-testgap-tail` supersedes `fact-test-suite-phase8`,
TASKS.md gap section rewritten as COMPLETED, INDEX synced (+1 node, 116).
