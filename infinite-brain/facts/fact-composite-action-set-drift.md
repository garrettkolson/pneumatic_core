---
id: fact-composite-action-set-drift
title: "The composite's admit-list was narrower than the committer's dispatch table — gossip died at the door and the chain never grew"
type: fact
namespace: architecture
visibility: namespace
summary: "10/08/2026, multi-host data-plane run: the mesh formed, txs routed, executors executed, finalizers finalized — and the committer chain grew by 0 of 200. `COMMITTER_ACTIONS` (node_server.rs) admitted only [\"Commit\"], while the adapter delegates every message to `Committer::handle_message` (committer.rs:304), whose real dispatch table owns seven actions including `BlockFinalized` — the gossip that CARRIES the finalized block to the committer. The dispatcher refused it as `UnknownAction` before the handler's own fail-closed sender auth could even see it. Symptom was 1200× 'data-plane dispatch failed: unknown action BlockFinalized' per node — surfaced only because the bridge fix (fact-rns-bridge-spawn-kills-worker) started logging dispatch failures; before that, gossip rejection was fully silent. Fixed: constants mirror the handler table verbatim, guard test pins it (mutation: revert → names the rehearsal in the failure text). Same drift family as the manifest `roles` wrong-bucket bug: a declared set that no test cross-checks against what the code actually runs."
auto_inject: false
applicable_when: "Adding wire actions, changing RoleHandler adapters or the *_ACTIONS constants, or debugging a pipeline whose hops run but whose chain does not grow"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "If COMMITTER_ACTIONS and Committer::handle_message's match arms ever diverge again (the guard test fires first); or if the composite gains an action-routing layer that derives the sets from the handlers instead of duplicating them"
tags: [fact, composite, role-dispatcher, gossip, committer, block-finalized, multihost]
edges:
  - target: concept-node-server-composite-runtime
    type: depends_on
    weight: 0.95
    note: "The RoleDispatcher's per-role action sets are the composite's admission control"
  - target: fact-rns-bridge-spawn-kills-worker
    type: preceded_by
    weight: 0.9
    note: "Only the logging added with that fix made the gossip rejections visible"
  - target: fact-manifest-declared-roles
    type: related_to
    weight: 0.85
    note: "Twin drift: a declared set nobody cross-checks against what the code actually runs"
  - target: concept-quorum-gossip-protocol
    type: related_to
    weight: 0.8
    note: "BlockFinalized is the carrier message this door refused"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.9
    note: "Second of three blockers found by Phase 2's exit attempt, in pipeline order"
---

# The composite door refused the gossip the committer was built to handle

## Symptom shape

Everything upstream looked alive: probe 12/12, counters climbing during the
200-tx run, no panics, no queue closures, `peers 12` sustained. The committer
chain grew by **0 of 200**. Only the (new) dispatch-failure logs showed the
corpuscles of the crime:

```
[pneumatic] data-plane dispatch failed: unknown action: "BlockFinalized"   ×1200
[pneumatic] data-plane dispatch failed: unknown action: "Clear"             ×600
```

## Two lists, one truth, no cross-check

`RoleDispatcher::dispatch` admits an action only if some installed role lists
it in the `*_ACTIONS` constants (node_server.rs:36-50). The committer
**adapter** delegates to `Committer::handle_message` — the real dispatch table
(committer.rs:304) owns `Commit, DistributeToken, DistributeBlock,
EpochReconcile, BlockFinalized, BlockConfirmed, BlockQuorumReached`, each with
its own sender-role gate (`allowed_senders_for`). The constant carried only
`["Commit"]`, so the block-carrying gossip never reached the handler. The
single-process e2e tests dispatch `Commit` directly — nothing ever delivered
`BlockFinalized` through the **composite dispatcher** until a real four-host
mesh tried.

`Clear`/`BlockFinalized` refusals at the SENTINEL/EXECUTOR/FINALIZER sides
are, by contrast, **correct** composite semantics: those adapters expose only
their pipeline entry (`on_data_received`, `ingest_preload`), and sentinel
monitoring / executor-clear are no-ops in a preload-implies-execution
composite. Fail-closed refusal is right; silence was not.

## The rule

The `*_ACTIONS` constants are an **admit-list over an adapter's capability** —
they must mirror what the delegated handler dispatches. Both lists live in
different crates; nothing typed enforces it; so the guard test does
(`committer_gossip_actions_reach_the_handler`): every action the committer
handler owns must arrive at the handler (never `UnknownAction`) — mutation-
verified by narrowing the constant back, whose failure message names this
rehearsal.
