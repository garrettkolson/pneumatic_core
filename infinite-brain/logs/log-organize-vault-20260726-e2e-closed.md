---
id: log-organize-vault-20260726-e2e-closed
type: log
operation: organize-vault
date: 2026-07-26T00:00:00
namespace: pneumatic
summary: "Closed the e2e pipeline integration test task. The test landed at tests/pipeline_integration.rs (e2e_standard_pipeline_commits_over_rns) and is green (~5.3 s). Root causes fixed: finalizer now signs blocks with the hybrid provider (Arc<NodeIdentity> → ed25519.sign_data(), 3796 B) matching the committer's hybrid check_signature; RNS Resource path requires a direct link (added SENT↔FIN edge to the 5-node topology); rns-net 0.7.0 port-wiring rule documented. All diagnostics stripped, DIRECT_PACKET_PLAINTEXT_MAX restored to 481, tests/rns_debug.rs deleted. Full workspace suite green."
affected_nodes: ["task-e2e-pipeline-integration-test", "concept-rns-transport", "_system/INDEX", "repo:TASKS.md"]
tags: ["log", "organize-vault", "e2e-pipeline", "rns"]
---

# Log: organize-vault — e2e pipeline integration test CLOSED

The e2e pipeline integration test (the audit's final Done-when) is now **landed and green**. Recorded the completion and the two RNS findings that unblocked the 5-node topology.

## What landed

`tests/pipeline_integration.rs` → `e2e_standard_pipeline_commits_over_rns` runs the full SUB→SENT→EXEC→FIN→COMM standard-tx chain over **5 RNS nodes** (9 identities, 2 instances per role sharing an anchor node) with real UDP loopback and per-node `on_packet` bridges. Runtime ~5.3 s. Both committers commit the identical block hash; sentinels receive `Clear`.

## Root causes fixed (this + prior sessions)

1. **Hybrid finalizer signature** (the `InvalidFinalizerSignature` blocker): the finalizer previously signed blocks with a plain 64 B Ed25519 signature, but the committer's hybrid `check_signature` expects the 3796 B `[Ed25519 64 B · ML-DSA-PK 1312 B · ML-DSA-sig 2420 B]` form and returns `Ok(false)` on a short sig. Fix: `BlockBuilder` + `Finalizer::new` now take `Arc<NodeIdentity>` and sign via `identity.ed25519.sign_data()`. All 6 other `Finalizer::new` call sites updated (plugins.rs, shielded.rs, helpers.rs, shielded_pipeline.rs).
2. **RNS Resource path needs a direct link**: the original 5-node topology was a line (SUB–SENT–EXEC–FIN–COMM). The finalizer's `Clear` fan-out targets the sentinels (FIN→SENT), but FIN and SENT were not directly connected, and the Resource transfer does **not** multi-hop — so the `Clear` never arrived. Fix: add a **SENT↔FIN** edge (small mesh).
3. **rns-net 0.7.0 port-wiring rule** (root cause of all earlier multi-node link failures): node D's UDP interface `k` listens on `base+k` and must forward to `peer_base + j`, where `j` is D's index **in the peer's** peer list — not `peer_base`.

## Cleanup

All temporary `[DIAG]`/`eprintln` diagnostics stripped (finalizer message_dispatcher, sentinel processing, RNS wrapper send_data_packet/on_resource_received/on_resource_completed, test bridges + size prints). `DIRECT_PACKET_PLAINTEXT_MAX` restored to **481** (production value). `tests/rns_debug.rs` deleted. Unused `random_signing_key()` helper removed. Full `cargo test` green.

## Nodes updated

- `task-e2e-pipeline-integration-test` → CLOSED (title, summary, confidence 1.0, completion section with the two RNS findings).
- `concept-rns-transport` → the "open gap" (no auto-routing) is now **resolved** via `RnsNetwork::send_data_packet` (wrapper.rs:440-452); confidence bumped; edge to the task added.
- `_system/INDEX` → task + concept row summaries updated.
- `repo:TASKS.md` → "Remaining test gaps" edited to remove the e2e pipeline item; Phase-6 e2e line marked COMPLETE.
