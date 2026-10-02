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
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "If registration message shapes, per-type collections, stake gate, or priority selection change; or if peering.rs stops being the only control-plane sender"
tags: [registry, nodes, registration, peer-discovery]
edges:
  - target: fact-control-plane-peering
    type: related_to
    weight: 0.95
    note: "The sender half of this protocol, landed 10/02/2026: Register/Request/Heartbeat on a retry loop"
  - target: fact-port-allocations
    type: related_to
    weight: 0.8
    note: "Each NodeRegistryType maps to dedicated external/internal ports (conns.rs:226-237)"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.8
    note: "Registration rides the MsgPack NetworkPacket control+data envelope (node.rs:36-43 area, node/registry/registration.rs:214)"
  - target: event-node-server-composite
    type: related_to
    weight: 0.7
    note: "Multi-bucket admission (node/registry/registration.rs:258-293) is what lets one composite node register under several types"
  - target: concept-env-driven-config
    type: depends_on
    weight: 0.7
    note: "Full vs Light participation set (config.rs:246-257) and per-type max capacity/min stake (config.rs:213-225) come from Config"
related: []
source_url: "Empty"
---

# NodeRegistry: per-type peer directories + signed registration protocol

`NodeType` is `Full`/`Light` (`src/node.rs:15`); `NodeRegistryType` is the five role buckets: `Committer, Sentinel, Executor, Finalizer, Archiver` (node.rs:141-147). `NodeRegistry` (`src/node/registry.rs:26`) holds one `DashMap<Vec<u8>, NodeRegistryNode>` **per type** (lines 27-31), keyed by node public key, plus liveness/eviction state (1 s poll, line 81), a 5 s per-send bound (line 75), and per-(rhash, type) delivery-failure counters (lines 88-102).

Registration is a signed, two-way protocol. A `NodeRequest` (node.rs:162) carries `requester_key`, a **claimed** transport address `requester_rhash`, requested type(s), and a `binding_signature` — an Ed25519 signature over `(rhash, requested_type, requester_types)`: forging the claim needs the victim's signing key (node.rs:149-171). `handle_register` (node/registry/registration.rs:241) verifies the binding first (line 247), admits the key under **every** qualifying type it declares — stake gate runs outside the lock (line 340), then capacity-check + insert run atomically under `admission_lock` (lines 347-368) — and replies with a binding-signed `RegisterAck` (lines 430-437); the acked type is the highest-priority one registered (Finalizer > Executor > Sentinel > Committer, lines 298-306). Directory responses sign the full `(entries, type, rhash)` tuple so signatures can't replay across types (node.rs:174-186).

Fan-out is `send_to_all`/`send_to_all_blocking` over the registered connections (`node/registry/fanout.rs:13`, `:87`); a node that arrived via a directory response (no own binding) is never listed in one (`node/registry/registration.rs:163-166`).

**The registry speaks, not just answers (10/02/2026).** Everything above was the receiving half; nothing in production ever *sent* a control message, so the sender side was unaudited in four ways (`fact-control-plane-silent-drop-paths`). `node/registry/peering.rs` is now that half: `Register` to every bootstrap peer, a directory `Request` per routable role bucket, then a 10 s loop of re-`Register` + `Heartbeat` (eviction deletes entries unseen for 30 s, so a cluster without this dissolves its own directories). It announces the registry's `declared_roles` — the roles this node actually runs, set by the composite from its installed plugins — rather than `config.node_registry_types`, because a receiver admits a node under *every* type it declares, so over-declaring files a node in buckets whose traffic it cannot serve. `build_directory_response` now also requires the requester's binding to verify and the requester to be registered; it previously answered any caller, which was never exercised until something started asking. See [[fact-control-plane-peering]].
