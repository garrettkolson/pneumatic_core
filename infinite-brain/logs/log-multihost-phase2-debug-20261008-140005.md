---
id: log-multihost-phase2-debug-20261008-140005
type: log
operation: multihost-phase2-debug + fact-authoring
date: "2026-10-08T14:00:05"
namespace: pneumatic
summary: "Phase 2 data-plane debug on the 4-container rehearsal: killed the last two blockers found (RNS-bridge reactor panic evicting the mesh; composite admit-list drift), fixed the finalize path's cold-token miss with a mutation-tested warm-on-miss, then found the final structural blocker (fan-out loses the target role on a multi-role host); mesh now survives a 200-tx burst, chain still 0 pending a design call"
affected_nodes: ["fact-rns-bridge-spawn-kills-worker", "fact-composite-action-set-drift", "fact-composite-fanout-role-collision", "fact-peering-late-join-convergence", "task-multihost-testnet-rollout", "_system/INDEX.md"]
tags: ["log", "multihost-phase2-debug", "fact-authoring", "composite", "data-plane", "role-dispatcher"]
---

The session started where formation debug had stalled and chased the data plane to
the floor, one measured root cause at a time. First the mesh itself: the first
cross-host data frames triggered `tokio::spawn` inside the RNS bridge's
`on_packet` — a std-thread callback with no reactor — panicking a worker per node,
closing inbound queues, and self-evicting every directory (peers 12→0 with health
still 200). Fix: `spawn_data_dispatch` on a runtime `Handle` captured at bridge
build time; the guard test calls it from a bare `std::thread` and the mutation
(bare `tokio::spawn`) reproduces the rehearsal panic verbatim. Mesh then held
12/12 through a 200-tx burst, zero panics.

Second, gossip admitted nowhere: `COMMITTER_ACTIONS=["Commit"]` vs the handler's
seven-action table — `BlockFinalized` died as `UnknownAction` before the handler's
own auth could speak. Constants now mirror the handler; guard test mutation-
verified.

Third (this span): `TokenNotFound("0a")` ×300/node — genesis sat in every sidecar
while the committer's `handle_block_finalized` used a bare `tokens.get_mut`; the
warm-on-miss rule `commit_block` learned in Phase 1 was never copied to the
finalize path because the single-composite e2e always warmed via the shared
registry first. Fix mirrors block_services.rs exactly;
`block_finalized_warms_a_cold_token_from_the_data_service` fails with the
rehearsal's own error when the warm is disabled.

Fourth — the stop: with committer-side errors gone, every `Sign` still died
`TransactionNotInFinalizing`. Reading the hop chain: the executor forwards the
stamped tx to the Finalizer bucket under the SAME action name ("Preload") the
sentinel's raw-tx hop uses; `RoleDispatcher` admits an action to exactly one
owner, so multi-role hosts silently route the finalizer's Preload into the
executor adapter. Wire frames carry no fan-out target; per-host placement of
multiple roles is unimplementable for reused action names until a design call
(distinct hop action / wire target / multi-owner delivery). Fact node authored
with the three shapes and the two-process test the fix must satisfy; task scorecard,
INDEX (+1 fact → 51, header, summary line) synced. Cluster left running on
pneumatic:phase2i; exit test unpassed — reported to the human with the decision.
