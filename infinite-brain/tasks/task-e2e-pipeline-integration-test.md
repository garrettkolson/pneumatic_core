---
id: task-e2e-pipeline-integration-test
title: "DONE: e2e pipeline integration test (sentinel → executor → finalizer → committer)"
type: task
namespace: pneumatic
visibility: namespace
summary: "CLOSED 07/26/2026: full-pipeline integration test landed at tests/pipeline_integration.rs — SUB→SENT→EXEC→FIN→COMM over RNS (5 RNS nodes, 9 identities, 2 per role); standard tx path with committer consensus. Closes the audit's Done-when scenario."
auto_inject: false
applicable_when: "Reviewing e2e pipeline coverage or the RNS multi-node topology findings"
confidence: 1.0
verified_at: "07/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale if the test is removed or the RNS topology rules change"
tags: [task, testing, integration, pipeline, complete, rns, e2e]
edges:
  - target: source-tasks-md
    type: derived_from
    weight: 1.0
    note: "Listed in 'Remaining test gaps'; gap list edited to remove it on 07/26/2026"
  - target: source-audit-checklist
    type: related_to
    weight: 0.8
    note: "Closes Done-when #3 (lines 1459-1460): multi-process e2e over the real wire"
  - target: concept-transaction-lifecycle
    type: related_to
    weight: 0.8
    note: "The test exercises the whole state machine"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.7
    note: "Validates the commit/finalize path on the block lattice"
  - target: concept-rns-transport
    type: related_to
    weight: 1.0
    note: "Test topology is 5 RNS nodes; findings: Resource path needs a direct link (FIN↔SENT edge added) + rns-net 0.7.0 port-wiring rule"
related: []
source_url: "repo:tests/pipeline_integration.rs"
---

# Task: e2e pipeline integration test

`TASKS.md` §"Remaining test gaps" (lines 912–913) lists the **e2e pipeline integration test** (sentinel → executor → finalizer → committer) as an open test gap, and it is the most consequential one: it is the only listed gap that exercises the entire transaction pipeline as one unit.

It is also the closing criterion of the audit remediation: AUDIT_CHECKLIST.md's overall **Done-when** (lines 1455–1460) requires "a clean multi-process (≥ 2 nodes per role) run completes a transaction end-to-end over the real wire path — the scenario the audit found inoperable." So this single test is the last piece standing between the current state and the audit's declared finish line.

The shielded S6.1 pipeline test (`tests/shielded_pipeline.rs`) follows the same fixture conventions as `committer/tests/pipeline_integration.rs`, so landing the public-pipeline version first also de-risks the shielded e2e work.

## Design (scoped 09/25/2026)

**Topology: 8 in-process nodes over real RNS — ≥ 2 per role.** The "real wire path" is RNS (see `concept-rns-transport`): 2 sentinels + 2 executors + 2 finalizers + 2 committers, each node its own `RnsNetwork` (own `NodeIdentity`, own UDP port), its own `NodeRegistry` seeded with `RnsConnection` peers (bootstrap from each peer's 64-byte RNS public key, symmetric topology), and its own `on_packet` bridge mirroring `node-server/build.rs:219-238` (`NetworkPacket` → control/data plane → the node's role handlers). Pattern to mirror: the 7.1 RNS loopback tests (`committer/tests/pipeline_integration.rs:1172+`). "Multi-process" is satisfied at the topology level: 8 independent node instances with independent state over real UDP sockets (the worker crates have no binaries — only `committer` has `main.rs` — so literal OS-process fan-out is not possible without new binaries).

**Two production blockers found during scoping (both must be fixed for the test to pass):**

1. **Wire MTU auto-routing (audit 7.1 residual)**: `RnsConnection::send` → `RnsNetwork::send_to` uses the **direct packet path only** (~481 B plaintext cap), while every real pipeline `Message` is ~3.8 KB (PQC hybrid signature). Fix: size-based auto-routing in `send_to` → `send_resource_to` when the payload exceeds the direct MTU (consistent with the 7.1 remediation; no wire-shape change).
2. **Executor → finalizer seam (C1 contract)**: the executor emits action `"Execute"` with an `ExecutionResult`, but the finalizer's C1 voter intake (`handle_signature`) expects action `"Sign"` with a `TransactionSignature` whose inner signature covers `result_hash` — and `"Execute"` is not in `FINALIZER_ACTIONS` (node_server.rs:49), so composite deployments reject the executor's result as `UnknownAction`. Fix: the executor signs `result_hash` with its identity key and emits a `Sign` vote (the contract `handle_signature` implements).

**Flow under test (standard, non-shielded path — the shielded path bypasses the executor by design):** sender → S1 `Process` (envelope `public_key == tx.sender`, sender sig verified) → sentinel validates (Executed spec) + registers pending + fans `Preload` (body = serialized Transaction) to both executors → each executor `preload_for_transaction` → stub execution → `Sign` (signed `TransactionSignature`) to both finalizers → first valid vote triggers `try_finalize_optimistic` → `Commit` + `BlockFinalized` to both committers + `Clear` to sentinels → assert both committers committed the **identical block hash** (H12 tx-hash check passed) and sentinels received `Clear`. Plus a C1 adversarial case: a forged voter key is rejected.

**Fixture requirements (from the role APIs):** sender user registered in DataProvider; token with a registered `ExecutedBlockValidatorSpec`; stake snapshots for the epochs the roles read; the data provider's `latest_block_hash` (chain tip) for the finalizer's `previous_hash`; committer tokens registered via BlockServices; per-node `PendingTransactionRegistry` (the executor reads the tx from **its own** registry, so each executor's registry must be populated from the received `Preload` body); the composite's seam where the executor adapter treats `message.body` as raw tx_id bytes is sidestepped by driving `preload_for_transaction(tx_id)` from the received Preload message (tx_id is known to the test).

**Root crate `Cargo.toml`**: add `pneumatic_executor = { path = "executor" }` to `[dev-dependencies]` (sentinel/finalizer/committer/prover are already there).

## CLOSED — 2026-07-26

The test landed at `tests/pipeline_integration.rs` (`e2e_standard_pipeline_commits_over_rns`) and is green (~5.3 s). It runs the full SUB→SENT→EXEC→FIN→COMM standard-tx chain over **5 RNS nodes** (9 identities, 2 instances per role sharing an anchor node) with real UDP loopback and per-node `on_packet` bridges. Finalizer now signs blocks with the **hybrid** provider (`Arc<NodeIdentity>` → `ed25519.sign_data()`), matching the committer's hybrid `check_signature` (3796 B) — this fixed the `InvalidFinalizerSignature` root cause.

**Two RNS findings that blocked the 5-node topology (now fixed / documented):**
1. **Resource path needs a direct link.** The original topology was a line (SUB–SENT–EXEC–FIN–COMM). The finalizer's `Clear` fan-out targets the sentinels (FIN→SENT), but FIN and SENT were not directly connected, so the Resource transfer (which does not multi-hop) never arrived. Fix: add a **SENT↔FIN** edge (small mesh).
2. **rns-net 0.7.0 port-wiring rule.** Node D's UDP interface `k` listens on `base+k` and must forward to `peer_base + j`, where `j` is D's index **in the peer's** peer list — not `peer_base`. Wrong wiring makes the link handshake silently fail.

`DIRECT_PACKET_PLAINTEXT_MAX` restored to 481 (production value); all temporary diagnostics stripped; `tests/rns_debug.rs` deleted. Full workspace suite green.
