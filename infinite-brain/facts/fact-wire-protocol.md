---
id: fact-wire-protocol
title: "Wire protocol: 4-byte BE length + MsgPack, 16 MB frame cap"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Inter-service frames = 4-byte big-endian length header + MsgPack (rmp-serde) payload; MAX_FRAME_SIZE = 16 MB enforced by senders."
auto_inject: false
applicable_when: "Implementing network I/O, debugging frames, or sizing payloads"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the frame header changes, rmp-serde is replaced, or MAX_FRAME_SIZE changes"
tags: [wire-protocol, msgpack, networking, framing]
edges:
  - target: pillar-block-lattice
    type: part_of
    weight: 0.7
    note: "Transport substrate of the protocol"
  - target: fact-port-allocations
    type: related_to
    weight: 0.8
    note: "Frames flow over the per-type ports"
  - target: fact-rns-pinning
    type: related_to
    weight: 0.8
    note: "RNS rides the same 16 MB cap (RESOURCE_MAX_BYTES = MAX_FRAME_SIZE)"
related: []
source_url: "Empty"
---

# Wire protocol: 4-byte BE length + MsgPack

Data frames consist of a **4-byte big-endian length header** followed by the MsgPack-serialized payload (rmp-serde). Reading is two-step: read 4 bytes for the length, then read exactly that many bytes (`src/conns/` — readers/senders).

A hard cap `MAX_FRAME_SIZE` = **16 MB** is enforced in the senders (`src/conns/senders.rs:92,144`) with an explicit "Frame size exceeds maximum" error. The RNS transport layer inherits this cap: `RESOURCE_MAX_BYTES: u64 = MAX_FRAME_SIZE` (`src/rns/wrapper.rs:83`), and RNS messages are rejected if larger than `MAX_FRAME_SIZE + ENVELOPE_MARGIN`.
