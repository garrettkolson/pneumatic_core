---
id: fact-control-plane-peering
title: "Peering initiator: the control plane now has a client half"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/02/2026: `src/node/registry/peering.rs` sends the control plane (Register / directory Request / Heartbeat) on a retry loop from both binaries. A node advertises `declared_roles` — the roles it actually runs, not config's `node_registry_types` — and a control frame's ~5.9 KB size means peering can only begin after the peer's announce makes its route live."
auto_inject: true
applicable_when: "Any multi-node run, launcher/peering work, or debugging a cluster where nodes boot but no traffic flows between roles"
confidence: 1.0
verified_at: "10/05/2026"
verified_by: "dsh-agent"
staleness_signal: "If peering.rs changes shape, if the binding signature size changes (moving control frames under the direct-packet cap), or if a binary stops calling start_peering"
tags: [fact, peering, control-plane, node-registry, rns, testnet, boot]
edges:
  - target: concept-node-registry
    type: related_to
    weight: 0.95
    note: "This is the sender half of the Register/RegisterAck protocol the registry already implemented"
  - target: concept-rns-transport
    type: depends_on
    weight: 0.9
    note: "Control frames exceed the direct-packet cap, so peering rides the Resource path and needs a live route"
  - target: fact-static-binding-replay
    type: related_to
    weight: 0.85
    note: "The directory Request is the one of the three control messages that repeats, so it alone carries a responder and a nonce"
  - target: fact-control-plane-silent-drop-paths
    type: related_to
    weight: 0.85
    note: "The four silent-failure paths this had to fix before a control message could cross a socket"
  - target: fact-register-ack-bucket-placement-defect
    type: related_to
    weight: 0.8
    note: "Sending Register is what exposed the ack's wrong-bucket handling"
  - target: fact-data-service
    type: preceded_by
    weight: 0.7
    note: "The data service gates every node's boot; peering is what gates their ability to talk to each other"
  - target: task-testnet-launcher
    type: supports
    weight: 0.9
    note: "Peering was the load-bearing blocker for a multi-node testnet"
related: ["[[NodeRegistry: per-type peer directories + signed registration protocol]]", "[[RNS transport: the production inter-node wire (RnsNetwork over rns-net, Resource transfer for large payloads)]]"]
source_url: "Empty"
---

# Peering initiator: the control plane now has a client half

Code- and socket-verified 10/02/2026. Before this, **no production code sent a
control-plane message**: `register_peer` had test-only callers,
`handle_register_ack` had never run outside a unit test, and the one
control-shaped send (the committer's announce handler) was mis-framed. A
multi-process cluster therefore booted cleanly with every role directory empty
and no error anywhere — see [[fact-control-plane-silent-drop-paths]].

**What it does** (`src/node/registry/peering.rs`, wired into both binaries):
`Register` to every bootstrap peer, a directory `Request` for each routable role
bucket, then a 10 s loop of re-`Register` (idempotent, refreshes liveness, and
re-adds us after a peer restarts) plus `Heartbeat` to peers learned from
directories. Without the loop the 30 s eviction cutoff dissolves a cluster's
directories half a minute after boot.

**`declared_roles`, not `config.node_registry_types`.** A receiver admits a node
under *every* type it declares that clears the stake gate. Config declares all
four types for any full node, so sending it verbatim would put a committer-only
binary into every peer's Finalizer/Executor/Sentinel bucket — where it would
receive `Sign`/`Preload`/`Process` traffic it cannot serve, and where a
role's quorum math would count a participant that never answers. So the registry
holds `declared_roles` (seeded from config, overridden by the composite with its
installed role plugins and by the committer with `[Committer]`), and every
outbound message — including a `RegisterAck` this node sends — describes itself
with that set. It is read per loop round, so an epoch's role change is
advertised without restarting the loop.

**The boot-ordering constraint, measured.** A control frame is **5.9 KB**
(`Register` 5932 B, `Heartbeat` 5977 B, directory `Request` ~5.9 KB,
`RegisterAck` 5960 B), because `sign_binding` produces the hybrid
`[Ed25519 sig · ML-DSA pk · ML-DSA sig]` (~3.8 KB) in `requester_key` +
`binding_signature`. The direct-packet plaintext cap is
`DIRECT_PACKET_PLAINTEXT_MAX = 481` B, so **a control frame can never ride a
direct packet** — it requires the Resource path, which requires a *live* route,
and a route seeded from `bootstrap_peers` is explicitly not live
(`received_at: 0.0` until a real announce arrives). So:

> **A node cannot peer at t=0.** It peers after it hears an announce from the
> peer. `send_control` returns "no live route yet" instead of blocking (the
> blocking `send_resource_to` waits 30 s per peer, which would stall the loop
> for minutes on a cold cluster), and peering is a retry loop. A cluster whose
> nodes never announce to each other can never register, however the messages
> are framed.

**Proof over a socket** — `tests/peering_e2e.rs` starts two real `RnsNetwork`
nodes on UDP, waits for both routes to go live, runs the peering loop, and
asserts each node lands in the other's *correct* role bucket (asymmetric roles,
so the ack-placement defect would fail it). Passes in 0.8 s, stable across
repeated runs. 16 unit tests cover the builders, the bucket math, the frame
shapes, and the directory gates.

## Update 10/05/2026 — the directory `Request` is no longer shaped like the others

`Register` and `Heartbeat` still sign the classic three-tuple binding. `Request`
does not: it now carries `query_target` (the responder's rhash) and `query_nonce`,
and signs the five-tuple `query_payload` instead. The reason is that of the three
messages it is the only one a peer sends **repeatedly** and whose answer is
**expensive** — see `fact-static-binding-replay`.

Callers of `build_directory_request` must now pass the responder's rhash; it is not
derivable from the requester's state. The fields on `NodeRequest` are
`#[serde(default)]`-ed `Option`s, so a peer on an older build is still understood —
with one caveat worth knowing before a rolling upgrade: rmp serializes structs
positionally, so the *frame* is not byte-compatible across the change even though
the semantics are backward-honored. Everything on a network has to move together.

`request_directories_from_peer(peer_rhash)` threads the peer's rhash straight
through, so the production peering loop needed no logic change — a query is always
addressed to the peer it is being sent to.
