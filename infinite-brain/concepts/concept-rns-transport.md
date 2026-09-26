---
id: concept-rns-transport
title: "RNS transport: the production inter-node wire (RnsNetwork over rns-net, Resource transfer for large payloads)"
type: concept
namespace: pneumatic
visibility: namespace
summary: "RNS is the production inter-node wire for all role-to-role traffic. RnsNetwork wraps pinned rns-net (4 workers, 200 ms poll); the direct packet path caps at ~481 B, so real ~3.8 KB Messages auto-route to send_resource_to via send_data_packet (resolved 07/26)."
auto_inject: false
applicable_when: "Touching src/rns/, any inter-node messaging design or test, large-payload send/receive, link negotiation, or diagnosing RNS send failures"
confidence: 0.98
verified_at: "07/26/2026"
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
  - target: task-e2e-pipeline-integration-test
    type: related_to
    weight: 0.9
    note: "The 5-node e2e test resolved the auto-routing gap and documented the Resource-path direct-link + port-wiring requirements"
related: []
source_url: "Empty"
---

# RNS transport: RnsNetwork wrapper

**RNS is the production inter-node wire.** All role-to-role traffic (sentinel → executor → finalizer → committer) rides RNS, not the TCP/UDS `conns` layer (which is the legacy/local layer for data-service channels and the original design — see `concept-conn-abstraction`). Evidence:

- The NodeRegistry registration path (node/registry/registration.rs:284, 418, 554) creates **`RnsConnection`** peers — the only production `Connection` implementation (every other `impl Connection` in the repo is a test double: `RecordingConnection`, `NoOpConnection`, `FailingConnection`, `HangingConnection`).
- The composite (node-server/build.rs:219-238) wires `RnsNetwork::on_packet` → deserialize `NetworkPacket` → control-plane (`registry.handle_control`) / data-plane (`route_data_plane` → `RoleDispatcher`).
- Outbound: roles call `node_registry.send_to_all(payload, &Type)` → `RnsConnection::send` → `RnsNetwork::send_to(rhash, payload)`.

So **integration tests of the pipeline must build their topology over RNS** (real `RnsNetwork` instances, own identity + UDP port per node, `RnsConnection` peers, `on_packet` dispatch) — not over raw TCP sockets. The 7.1 RNS loopback tests (`committer/tests/pipeline_integration.rs:1172+`) are the pattern: `NodeIdentity::generate_in_memory()`, `RnsNodeConfigBuilder::new().with_udp_port(port).add_peer(...)`, `RnsNetwork::start(config, &identity, &bootstrap_peers)`, bootstrap-seed the peer's rhash from its 64-byte RNS public key (symmetric topology required for the announce handshake), `announce()`, then `send_to`.

`RnsNetwork` (src/rns/wrapper.rs) is pneumatic's facade over the pinned rns-net transport. The module docs (wrapper.rs:19-44) record the design constraints:

- **Constants**: `APP_NAME = "pneumatic"`, `ASPECTS = ["udp", "pneumatic"]`, `WORKER_THREADS = 4` with `WORKER_POLL_INTERVAL = 200 ms`.
- **~481 B packet cap**: rns-core's MTU (500 B) minus the 19-byte HEADER_1 leaves ~481 B of plaintext per direct packet. Payloads larger than that **cannot** go through the direct send path; the wrapper provides a separate **Resource transfer** API (`send_resource_to`, wrapper.rs:436) that ships the payload in the resource's opaque `data` field (SDU fragments of ~464 B reassembled by the receiver), after a one-time **link negotiation** (`register_link_destination` + `create_link`).
- **✅ Resolved (07/26, e2e pipeline task)**: Oversized payloads now auto-route via `RnsNetwork::send_data_packet` (wrapper.rs:440-452): it wraps the payload in a `NetworkPacket{data}` frame and, if the serialized frame exceeds `DIRECT_PACKET_PLAINTEXT_MAX` (481 B), delegates to `send_resource_to`; otherwise it uses the direct `send_to` path. A real pneumatic `Message` serializes to ~3.8 KB (the PQC hybrid signature alone is 3796 B: 64 Ed25519 + 1312 ML-DSA-PK + 2420 ML-DSA-sig), so every real pipeline message now rides the Resource path. This closes the inoperability audit 7.1 found. (The legacy `RnsConnection::send` → `send_to` direct-only path is retained for the local/data-service layer; role traffic goes through `send_data_packet`.)
- **Link cache**: negotiated `link_id`s are cached by destination rhash in the `links` table so the negotiation happens once per peer; `set_resource_receive_mode(AcceptAll)` arms the receiver side.
- **DoS guard**: the raw-size check rejects oversized Resource frames before worker processing.

`RnsConnection` (src/rns/conn.rs:1-34) is the thin `Connection`-trait adapter: it holds an `Arc<RnsNetwork>` plus the peer's 16-byte rhash and forwards `send()` to `network.send_to(rhash, data)`. The wrapper imports `crate::conns::MAX_FRAME_SIZE` (wrapper.rs:63) so RNS payloads inherit the workspace-wide 16 MiB frame cap, and `send_resource_to` is **fail-closed**: sending before a link exists returns `PneumaticError::Resource(_)` rather than a partial/lost payload (regression test wrapper.rs:780-794).
