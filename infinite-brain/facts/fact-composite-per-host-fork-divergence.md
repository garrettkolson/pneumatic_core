---
id: fact-composite-per-host-fork-divergence
title: "Every composite commits its own fork: the optimistic finalize path produces one block per host and the commit layer resolves it locally"
type: fact
namespace: architecture
visibility: namespace
summary: "10/08/2026, surfaced the moment self-delivery landed and commits started landing. On the 4-host × 4-role rehearsal each host's own Finalizer optimistic-finalizes the same transaction and books its OWN block (the block hash binds the build timestamp, so the four hashes differ by construction), and each host's Committer accepts exactly the copy whose hash matches its local booking — the P10 `wire_authoritative` path — while refusing the three peers' copies (`acquire_fail … booked ≠ wire` HASH_DIFFER, or `BlockValidationError(ChainLinkage)` when the peer block links elsewhere). Final 15-tx run: COMMIT-OK 15–18 per node, acquire_fail 28–71, ChainLinkage rejects 0–34 (sentinel-1 saw none — its own copies won every slot). Worse than 'different chains': a peer fork can REPLACE a committed local tip (`Token::commit_block` with `rollback_tip_hash` removes the loser and appends the winner), so one host commits the same tx into two different blocks and `Token::sequence_number` counts appends, not transactions. Delivery per host works; the cluster has no single committed chain."
auto_inject: false
applicable_when: "Designing cross-host block agreement, leader proposal or finalizer quorum for the optimistic path; reading per-node COMMIT-OK/ChainLinkage tallies; interpreting a token's commit counter as a delivery measure; anything that assumes 'the chain' is one object across hosts"
confidence: 0.85
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "When the commit layer stops resolving conflicts locally — i.e. a block for a token is produced by an agreed producer (leader proposal actually drives the optimistic path) or a cross-host quorum decides which block a slot holds; re-verify by re-running the 4-host rehearsal and checking that per-node COMMIT-OK counts no longer exceed the transaction count and ChainLinkage/acquire_fail HASH_DIFFER tallies stop being the normal case"
tags: [fact, composite, finalizer, committer, fork, optimistic-finality, conflict-resolution, multihost, per-host-placement]
edges:
  - target: fact-composite-no-self-delivery
    type: depends_on
    weight: 0.9
    note: "Self-delivery had to land first: until a host could commit at all, the per-host-fork consequence was invisible behind 'chain 0 of N'"
  - target: decision-optimistic-finality
    type: depends_on
    weight: 0.9
    note: "ADR-005/010 — the per-tx optimistic path is what lets every host's Finalizer produce a block for the same tx"
  - target: decision-conflict-definition-and-resolution
    type: related_to
    weight: 0.9
    note: "The winner/loser rollback (H2, `rollback_tip_hash`) is the mechanism that lets a PEER fork replace an already-committed local tip"
  - target: decision-deterministic-leader-election
    type: related_to
    weight: 0.7
    note: "The unanswered question: who may produce the block for a slot when every host runs a Finalizer"
  - target: fact-fanout-graph-density
    type: related_to
    weight: 0.6
    note: "The full-mesh fan-out is why every host sees every other host's forked Commit for the same tx"
  - target: task-multihost-testnet-rollout
    type: relates_to
    weight: 0.95
    note: "Phase 2's remaining data-plane blocker once chain growth became possible"
---

# Four hosts, four chains: per-host optimistic finality has no cross-host resolution

## What the rehearsal showed

With self-delivery in place the same transaction (`ph2-*`, token `0a`) reaches a
committed block on **every** host — the pipeline works end to end. But the
tallies say the four hosts are not agreeing on one chain:

| node | COMMIT-OK | acquire_fail (HASH_DIFFER etc.) | ChainLinkage rejects |
|---|---|---|---|
| committer-1 | 17 | 28 | 34 |
| executor-1 | 18 | 41 | 18 |
| finalizer-1 | 17 | 46 | 19 |
| sentinel-1 | 15 | 71 | 0 |

for a 15-transaction run. Per tx, four Finalizer roles (one per host) each
optimistic-finalize and book the shared pending entry `Committed{own_block_hash}`;
block hashes differ because the hash binds the build timestamp (P10). Each host's
Committer then faces four `Commit` copies and accepts the one that matches its own
booking — the `wire_authoritative` branch in
`committer/src/committer/committing.rs:91-116` — refusing the other three.

So the P10 mechanism that made a composite able to commit at all is exactly the
mechanism that makes each host keep its own fork. The refusal of peer copies is
by design (fail closed); what is *not* designed is what happens next.

## The part that is worse than divergence

`Token::commit_block` takes `rollback_tip_hash` (AUDIT 5.2 / H2): when a
conflict-resolved winner arrives, the current tip is **removed** and the winner
appended. On this cluster that path fires between *peers*: a host can discard a
block it already committed and append another host's block for the same
transaction. Two consequences:

- one host commits the same tx twice into different blocks (observed: two
  `COMMIT-OK tx=ph2-926` lines with different `wire=` hashes on committer-1, with
  the booked hash changing between them);
- `Token::sequence_number` therefore counts *appends*, not transactions — it is a
  floor on delivery, not a per-transaction proof (see
  [[fact-token-chain-window-caps-delivery-gate]]).

No double-*delivery* was involved: no transaction was committed twice with the
same block hash on any node, which is the signature self-delivery duplication
would leave.

## What is and is not claimed

Claimed, from the run: per-host commit works; peer forks are refused locally;
committed content differs per host; a peer fork can replace a committed local
tip. Not claimed: that no convergence mechanism exists — the epoch layer has
`LeaderSelector`/`BlockProposer` and the committer has a leader-proposal branch,
neither of which is what drives this optimistic per-tx path. The open question
for Phase 2 is which of those owns block production for a token when every host
runs a Finalizer, and what the commit layer should do with a block it did not
produce.
