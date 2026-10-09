---
id: fact-composite-no-self-delivery
title: "A composite host cannot receive its own role-to-role messages: send_to_all buckets hold only peers, and the e2e harness's relay hid the gap"
type: fact
namespace: architecture
visibility: namespace
summary: "**RESOLVED 10/08/2026** (`2fb44dc`) — proven live: the 4-container rehearsal grew the chain with no test-side relaying, then pinned by a composite e2e with the harness relay REMOVED. Original finding, 10/08/2026, the true final Phase 2 blocker: every per-tx hop is a send_to_all to a role bucket; on a four-full-node composite every host IS that role too, but the bucket contains only the 3 REMOTE entries — nothing registers or loopbacks the host's own membership. Measured with 5 txs on committer-1: 3 Commit copies arrive (the peers'), 0 from its own finalizer role; its local optimistic-path booking (Committed + own block hash, P10) matches only the never-arriving self copy — 15/15 acquire_fail HASH_DIFFER, zero COMMIT-OK, chain 0 of 5. Delivery alone was not the whole fix: sender authentication is bucket-derived, so a self copy signed by the host's own identity resolved to zero roles and was refused as `Unregistered` — role resolution had to learn the self-subscription too."
auto_inject: false
applicable_when: "Any composite message-flow bug where a sender's own role never sees its fan-out; writing or reviewing composite e2e relays; implementing self-delivery; debugging 'handler works on peers but never locally'"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "RESOLVED 10/08/2026 and re-verify if: a role ever stops receiving its own host's fan-out (the self-subscription is installed in exactly one place — build.rs after role install); if a self copy is ever delivered twice for one fan-out (both guards must hold: host runs the role, host not already in the bucket); if a same-host sender is ever refused as Unregistered; or if a composite e2e grows a relay again — the no-relay test is the pin and must stay relay-free"
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

## Resolved 10/08/2026 (`2fb44dc`) — what actually landed

Fan-out-side self-dispatch through the **same inbound closure**, in
`src/node/registry/self_delivery.rs`. Of the two sketched shapes, the sink won:
the RNS wrapper has no local-delivery seam (`send_frame` errors "no route to
rhash", and sending to the host's own rhash kills the worker — see
[[fact-rns-bridge-spawn-kills-worker]]), whereas feeding the framed
`NetworkPacket` to `build_runtime`'s `on_packet` closure needs no transport at
all and traverses the identical parse, control branch, admit-lists and
dispatcher. `install_self_delivery(roles, sink)` is called by the composite
bridge and by nothing else; `set_declared_roles` deliberately does **not**
subscribe a host to its own fan-out (a standalone full-node config declares all
four types and must keep peer-only behavior).

Exactly-once is structural, not opportunistic: the host must **run** the target
role, and must **not** already be in the target bucket. The second guard is what
keeps the existing relay-convention fixtures — which register the node's own key
as a peer — from receiving both the recording-connection copy and a local copy.
Targeted sends are keyed, not typed: a local copy is owed only when the key list
names this host's own key, and an unserved self-target stays reported
undelivered so `NoTarget` still reaches the caller.

**The half the sketch missed: delivery is not enough.** Every inbound handler
authenticates its sender by asking the registry which role the key holds, and
that lookup is bucket-derived. A self copy signed by the host's own identity
therefore arrived and was refused as an `Unregistered` sender — the pipeline
stalled with the copy in hand. `roles_of_key` now unions the installed
self-roles when the key is this host's own. That is *true*, not permissive: an
envelope that verifies under this host's public key can only have been produced
by this host, and the roles it resolves to are exactly the plugins the bridge
installed on it. Any other key resolves unchanged, so `Unregistered` stays
reachable (pinned).

Proof, in the order this node demanded: the rehearsal grew the chain (0 → 5 with
no test-side machinery; see `deploy/multihost/run4-phase2d-evidence/NOTES.txt`),
then `a_composite_pipeline_commits_with_no_test_side_relay` pinned it — the
production pipeline with the relay removed and no bucket entry for the host's own
key. Both halves are load-bearing there: withholding the local copy stalls it
("no block committed … chain length = 0"), and so does dropping the own-key role
resolution (the copy arrives, refused as an unregistered sender). 12 core unit
tests, each mutation-verified; the 4-container run showed no transaction
committed twice with the same block hash on any node.

What self-delivery then **exposed**, rather than caused: the sentinel's
cross-sentinel `Clear` now arrives at its own host and is refused
(`unknown action: "Clear"`) because no installed role owns it — documented at
`SENTINEL_ACTIONS`, deliberately unfixed. And it made per-host fork divergence
visible for the first time: [[fact-composite-per-host-fork-divergence]].
