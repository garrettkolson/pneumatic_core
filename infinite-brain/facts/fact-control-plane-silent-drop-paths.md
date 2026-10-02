---
id: fact-control-plane-silent-drop-paths
title: "Four control-plane paths that dropped packets without an error"
type: fact
namespace: pneumatic
visibility: namespace
summary: "RESOLVED 10/02/2026: the control plane had four independent silent-failure paths — a bare NodeRequest sent outside a NetworkPacket, ~5.9 KB replies sent on the direct-packet path, a directory response that echoed a Request (unbounded reply loop between two peers), and a directory answered to any caller. All four were invisible because nothing had ever sent a control message."
auto_inject: true
applicable_when: "Adding or changing any control-plane send, or debugging a cluster where registration half-works"
confidence: 0.35
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "All four fixed since 10/02/2026; re-read if a new control send bypasses RnsNetwork::send_control_frame, or if a NetworkPacket reply starts carrying a control echo again"
tags: [fact, control-plane, rns, silent-failure, defect, resolved, testnet]
edges:
  - target: fact-control-plane-peering
    type: related_to
    weight: 0.95
    note: "All four were found by putting real traffic on a path that had never carried any"
  - target: concept-rns-transport
    type: depends_on
    weight: 0.8
    note: "Two of the four are size/framing rules of the RNS wrapper"
  - target: concept-node-registry
    type: related_to
    weight: 0.8
    note: "The directory echo and the unauthenticated directory oracle live in registration.rs"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.6
    note: "Three of the four failed silently instead of failing closed — the reason they survived"
related: ["[[RNS transport: the production inter-node wire (RnsNetwork over rns-net, Resource transfer for large payloads)]]"]
source_url: "Empty"
---

# Four control-plane paths that dropped packets without an error

Found and fixed 10/02/2026 while building the peering initiator
([[fact-control-plane-peering]]). Each one is individually plausible, and each
one produced *no observable symptom* because nothing in production had ever sent
a control message — the receiving halves were only ever called directly from
unit tests, where there is no framing, no size limit, and no second peer to loop
with.

**1. A bare `NodeRequest` on the wire (committer's announce handler).** It sent
`serialize_to_bytes_rmp(&NodeRequest{..})`. The receiver deserializes
`NetworkPacket`, and a named-map rmp payload with unknown keys decodes
*successfully* into `NetworkPacket { control: None, data: None }` — serde ignores
unknown keys and reads an absent `Option` field as `None`. So the packet decoded
"fine" and was dropped with no error and no log line. Pinned by the unit test
`bare_node_request_bytes_are_a_black_hole`. (The wrapper's own
`send_data_packet` docs describe the identical trap for `Message`s — this was
the control-plane copy of a bug already found once.)

**2. ~5.9 KB replies on the direct-packet path.** `reply_register_ack` and
`handle_request` both called `network.send_to(rhash, bytes)`, which is the
direct path only — capped at `DIRECT_PACKET_PLAINTEXT_MAX = 481` B, while these
frames are ~5.9 KB. The fix routes both through a new
`RnsNetwork::send_control_frame`, which shares the size rule with
`send_frame`/`send_data_packet` but returns "no live route yet" instead of
blocking 30 s in `send_resource_to` — packet-handler threads must not stall on a
peer that has not announced.

**3. The directory response echoed a `Request` — a reply loop with no
termination.** `handle_request` sent its answer as
`NetworkPacket { control: Some(NodeRequest{ request_type: Request, binding_signature: vec![] }), data: Some(response) }`.
Both binaries dispatch `control` and `data` **independently, not `else if`** — so
the receiver ran `handle_control(Request)` on its own answer and replied to it.
Two peers exchange directory responses forever, each round a multi-KB Resource
transfer, with no counter or stop condition. The response is now data-only.
Compounding it: `handle_request` verified *nothing*, so the echo was answerable.

**4. The directory was an open oracle.** `handle_request` answered any caller,
leaking the validator set (keys, transport addresses, bindings) to whichever UDP
peer asked. `build_directory_response` now requires the requester's binding to
verify **and** the requester to be registered here, checking the signature
before the lookup (same order as `handle_heartbeat`). A legitimate peer
registers first — the peering loop does exactly that — and a `Request` that
overtakes its own `Register` goes unanswered and is retried on the next tick.

The common shape worth remembering: **a protocol half that has never been
exercised end-to-end is not "untested but working", it is unaudited.** Three of
these were in code the vault described as complete and sound, with line-number
citations.
