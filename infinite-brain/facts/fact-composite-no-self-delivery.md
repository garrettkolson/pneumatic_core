---
id: fact-composite-no-self-delivery
title: "A composite host cannot receive its own role-to-role messages: send_to_all buckets hold only peers, and the e2e harness's relay hid the gap"
type: fact
namespace: architecture
visibility: namespace
summary: "10/08/2026, the true final Phase 2 blocker — proven arithmetically on the live cluster after the PreloadForFinalizer fix landed. Every per-tx hop is a send_to_all to a role bucket; on a four-full-node composite every host IS that role too, but the bucket contains only the 3 REMOTE entries — nothing registers or loopbacks the host's own membership. Measured with 5 txs on committer-1: 3 Commit copies arrive (the peers'), 0 from its own finalizer role; its local optimistic-path booking (Committed + own block hash, P10) matches only the never-arriving self copy — 15/15 acquire_fail HASH_DIFFER, zero COMMIT-OK, chain 0 of 5. Every node accepts only its own forked block and its own block never reaches it: chain growth is mathematically impossible. The single-composite e2e never saw it because the test harness manually relays every hop (the 'mesh stand-in', e2e.rs ~1570) — the tests ARE the missing self-delivery. Fix belongs in the bridge layer: self-subscription on the node's own destination (deliver a copy locally whenever a role's fan-out targets a type this host runs)."
auto_inject: false
applicable_when: "Any composite message-flow bug where a sender's own role never sees its fan-out; writing or reviewing composite e2e relays; implementing self-delivery; debugging 'handler works on peers but never locally'"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "When self-delivery lands: a committed block appears WITHOUT the harness relay — proven live (chain grows in the 4-container rehearsal with no test-side relaying), and a two-role one-process test that does NOT relay shows the local handler receiving the fan-out"
tags: [fact, composite, self-delivery, fanout, node-registry, per-host-placement, multihost]
edges:
  - target: fact-composite-fanout-role-collision
    type: related_to
    weight: 0.95
    note: "The collision fix (distinct action names) landed first and is necessary; it exposed this — the remaining half of per-host placement"
  - target: concept-node-server-composite-runtime
    type: depends_on
    weight: 0.9
    note: "The bridge/RNS layer is where self-subscription must land"
  - target: fact-rns-bridge-spawn-kills-worker
    type: related_to
    weight: 0.7
    note: "Same bridge layer; self-copies arrive on the same std-thread callbacks"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.95
    note: "The last known blocker of Phase 2's data-plane exit test"
---

# The message a node sends to its own role never arrives

## Mechanism

Every pipeline hop is `NodeRegistry::send_to_all(payload, Type)`. On the composite,
the buckets hold **registered peers only** (12 peers = 4 types × 3 remote nodes;
self is absent by construction — peering registers peers, and nothing else calls
`register_peer` with the host's own identity). So a role fanning out to a type
this same host also runs talks to everyone-but-itself.

## The arithmetic that convicts it (5-tx run, phase2m-diag, committer-1)

Per tx, four finalizer roles (one per host) each optimistic-finalize, book the
shared entry `Committed{own_block_hash}` (P10 — blocks differ per host because
block hash binds the build timestamp), and fan `Commit` to the Committer bucket.

- 3 `Commit` copies arrive at each node — the PEERS' forked blocks;
- each meets the booking at `acquire_transaction` → `acquire_fail state=Committed`,
  and **15/15 carry `booked ≠ wire` (HASH_DIFFER)** → `TransactionNotInFinalizing`;
- 0 copies from the node's own finalizer role — the ONLY copy whose hash matches
  the local booking (the `wire_authoritative` P10 path was built for exactly it);
- no `COMMIT-OK` anywhere; DS chain: **0 of 5**.

Every node will only ever accept its own block, and its own block is the one
message it cannot deliver to itself. This is why the chain could never grow on
this architecture regardless of every other fix.

## Why no test ever caught it

The composite e2e drives messages by recording each hop's fan-out and
**manually re-dispatching it** ("relay the remaining pipeline hops (mesh
stand-in)", e2e.rs ~1570). The harness is a self-delivery that stands in for a
missing production capability. Any green composite e2e is, on this point,
meaningless — only the live rehearsal (or a test without relaying) can prove it.

## Fix sketch (bridge-layer self-subscription)

The composite bridge already sees every inbound frame (`on_packet` →
`route_data_plane`). The symmetric outbound seam: when the node's fan-out is
about to travel the wire to type T and this host runs T, deliver one copy
through the SAME inbound path — by rhash, not by bucket: a self-send of the
node's own destination (wrapper loopback: `dest == self_rhash` ⇒ feed
`on_data(self_rhash, frame)` locally, no network). Either drive it from the
fan-out side (send_to_all also dispatches locally when the host holds the type)
or make self a first-class bucket entry backed by a loopback connection. It
must arrive exactly once per fan-out, hit the same dispatcher and the same
admit-lists, and never double on non-composite hosts (where the host is not in
the target bucket). The first proof is not a unit test: **the 4-container
rehearsal must grow the chain** — then pin it with the no-relay two-role test.
