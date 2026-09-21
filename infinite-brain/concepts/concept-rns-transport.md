---
id: concept-rns-transport
title: "RNS transport: RnsNetwork wrapper over rns-net with Resource transfer for large payloads"
type: concept
namespace: pneumatic
visibility: namespace
summary: "RnsNetwork wraps pinned rns-net (4 workers, 200 ms poll); payloads over the ~481 B packet cap go through negotiated RNS links and native Resource transfer."
auto_inject: false
applicable_when: "Touching src/rns/, large-payload send/receive, link negotiation, or diagnosing RNS send failures"
confidence: 0.95
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If src/rns/wrapper.rs changes worker count, link negotiation flow, or the 481 B packet-cap handling"
tags: [rns, reticulum, transport, resource, link, mtu]
edges:
  - target: fact-rns-pinning
    type: derived_from
    weight: 1.0
    note: "Wrapper behavior depends on the exact rns-net/rns-core versions"
  - target: fact-rns-nodeconfig
    type: derived_from
    weight: 0.9
    note: "RnsNetwork is constructed from the builder's single NodeConfig"
  - target: concept-conn-abstraction
    type: related_to
    weight: 0.8
    note: "RnsConnection (src/rns/conn.rs) is the third Connection implementation"
  - target: event-rns-e2e
    type: related_to
    weight: 0.8
    note: "E2E tests exercise this wrapper over real UDP"
related: []
source_url: "Empty"
---

# RNS transport: RnsNetwork wrapper

`RnsNetwork` (src/rns/wrapper.rs) is pneumatic's facade over the pinned rns-net transport. The module docs (wrapper.rs:19-44) record the design constraints:

- **Constants**: `APP_NAME = "pneumatic"`, `ASPECTS = ["udp", "pneumatic"]`, `WORKER_THREADS = 4` with `WORKER_POLL_INTERVAL = 200 ms`.
- **~481 B packet cap**: rns-core's MTU (500 B) minus the 19-byte HEADER_1 leaves ~481 B of plaintext per direct packet. Payloads larger than that **cannot** go through the direct send path; the wrapper routes them through rns-net's native **Resource transfer** (SDU fragments of ~464 B reassembled by the receiver), after a one-time **link negotiation** (`register_link_destination` + `create_link`).
- **Link cache**: negotiated `link_id`s are cached by destination rhash in the `links` table so the negotiation happens once per peer; `set_resource_receive_mode(AcceptAll)` arms the receiver side.
- **DoS guard**: the raw-size check rejects oversized Resource frames before worker processing.

`RnsConnection` (src/rns/conn.rs:1-34) is the thin `Connection`-trait adapter: it holds an `Arc<RnsNetwork>` plus the peer's 16-byte rhash and forwards `send()` to `network.send_to(rhash, data)`. The wrapper imports `crate::conns::MAX_FRAME_SIZE` (wrapper.rs:63) so RNS payloads inherit the workspace-wide 16 MiB frame cap, and `send_resource_to` is **fail-closed**: sending before a link exists returns `PneumaticError::Resource(_)` rather than a partial/lost payload (regression test wrapper.rs:780-794).
