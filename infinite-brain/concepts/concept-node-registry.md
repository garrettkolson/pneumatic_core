---
id: concept-node-registry
title: "NodeRegistry: per-type peer directories + signed registration protocol"
type: concept
namespace: pneumatic
visibility: namespace
summary: "NodeRegistry keeps per-type DashMap directories (Committer/Sentinel/Executor/Finalizer/Archiver) and runs a binding-signed Register/RegisterAck protocol with stake gates and capacity limits."
auto_inject: false
applicable_when: "Modifying node registration, fan-out delivery, peer eviction, multi-role (composite) nodes"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If registration message shapes, per-type collections, stake gate, or priority selection change"
tags: [registry, nodes, registration, peer-discovery]
edges:
  - target: fact-port-allocations
    type: related_to
    weight: 0.8
    note: "Each NodeRegistryType maps to dedicated external/internal ports (conns.rs:226-237)"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.8
    note: "Registration rides the MsgPack NetworkPacket control+data envelope (node.rs:36-43 area, registry.rs:443)"
  - target: event-node-server-composite
    type: related_to
    weight: 0.7
    note: "Multi-bucket admission (registry.rs:487-522) is what lets one composite node register under several types"
  - target: concept-env-driven-config
    type: depends_on
    weight: 0.7
    note: "Full vs Light participation set (config.rs:246-257) and per-type max capacity/min stake (config.rs:213-225) come from Config"
related: []
source_url: "Empty"
---

# NodeRegistry: per-type peer directories + signed registration protocol

`NodeType` is `Full`/`Light` (`src/node.rs:15`); `NodeRegistryType` is the five role buckets: `Committer, Sentinel, Executor, Finalizer, Archiver` (node.rs:141-147). `NodeRegistry` (`src/node/registry.rs:26`) holds one `DashMap<Vec<u8>, NodeRegistryNode>` **per type** (lines 27-31), keyed by node public key, plus liveness/eviction state (1 s poll, line 81), a 5 s per-send bound (line 75), and per-(rhash, type) delivery-failure counters (lines 88-102).

Registration is a signed, two-way protocol. A `NodeRequest` (node.rs:162) carries `requester_key`, a **claimed** transport address `requester_rhash`, requested type(s), and a `binding_signature` — an Ed25519 signature over `(rhash, requested_type, requester_types)`: forging the claim needs the victim's signing key (node.rs:149-171). `handle_register` (registry.rs:470) verifies the binding first (line 476), admits the key under **every** qualifying type it declares — stake gate runs outside the lock (line 569), then capacity-check + insert run atomically under `admission_lock` (lines 576-597) — and replies with a binding-signed `RegisterAck` (lines 659-666); the acked type is the highest-priority one registered (Finalizer > Executor > Sentinel > Committer, lines 527-535). Directory responses sign the full `(entries, type, rhash)` tuple so signatures can't replay across types (node.rs:174-186).

Fan-out is `send_to_all`/`send_to_all_blocking` over the registered connections (lines 864, 938); a node that arrived via a directory response (no own binding) is never listed in one (lines 392-395).
