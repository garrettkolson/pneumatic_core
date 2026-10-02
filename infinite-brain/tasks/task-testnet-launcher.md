---
id: task-testnet-launcher
title: "Testnet launcher: key/genesis generator, sparse-topology allocator"
type: task
namespace: pneumatic
visibility: namespace
summary: "PARTIAL. The peering initiator landed 10/02/2026 (see fact-control-plane-peering) — nodes now register with each other over UDP. Two pieces remain: pre-generated keystores feeding a centrally computed peer/port matrix, and a topology that respects RNS's point-to-point interface model for the 20-40 node target."
auto_inject: false
applicable_when: "Building or extending local testnet tooling, or debugging why a multi-node cluster does not route"
confidence: 0.85
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "If a launcher/generator exists that writes keystores and emits the peer+port matrix, or if relay (`transport_enabled: true`) is exercised in this repo"
tags: [task, testnet, peering, rns, topology, launcher, transport]
edges:
  - target: fact-data-service
    type: preceded_by
    weight: 0.9
    note: "The data service + genesis is the completed first step; it is the hard gate every node shares"
  - target: concept-node-registry
    type: depends_on
    weight: 0.9
    note: "The missing piece is the client side of its Register/RegisterAck protocol"
  - target: concept-rns-transport
    type: depends_on
    weight: 0.9
    note: "Interfaces are point-to-point: one bound UDP port per peer, and Resource frames never multi-hop"
  - target: fact-rns-nodeconfig
    type: depends_on
    weight: 0.7
    note: "The ~45-field NodeConfig and its baked constants bound what a launcher can vary"
  - target: task-e2e-pipeline-integration-test
    type: derived_from
    weight: 0.8
    note: "Its 5-node chain topology, UDP-block allocator, and j-rule port computation are the reusable seeds of the launcher"
  - target: concept-node-server-composite-runtime
    type: related_to
    weight: 0.7
    note: "Fixed 10/02/2026: build.rs now calls with_udp_port(config.rns_port); the composite still installs roles by stake"
related: ["[[NodeRegistry: per-type peer directories + signed registration protocol]]", "[[RNS transport: the production inter-node wire (RnsNetwork over rns-net, Resource transfer for large payloads)]]"]
source_url: "Empty"
---

# Testnet launcher: key/genesis generator, sparse-topology allocator

The user goal is a quick way to spin up a testnet of decent size (target stated:
20-40 validators, one host, full mesh). The data service + genesis
(`fact-data-service`) removed the universal boot gate, and the peering initiator
(`fact-control-plane-peering`, landed 10/02/2026) removed the networking gate:
both binaries now send binding-signed `Register`s to their bootstrap peers, ask
for role directories, heartbeat to keep directories alive, and land in each
other's correct buckets — proven by `tests/peering_e2e.rs` over real UDP.

Getting the initiator working also surfaced four silent control-plane failures
and one misrouting defect, all fixed; see
`fact-control-plane-silent-drop-paths` and
`fact-register-ack-bucket-placement-defect`. The composite also now honors
`rns_port` (it previously bound 4242 regardless, so a second same-host composite
"booted without transport"), and it handles directory responses, which it used to
drop.

Two blockers remain.

**1. (DONE 10/02/2026 — was: nothing populated a role registry from a peer.)**
Historical record, because the shape of the gap is instructive: the server half
existed and looked sound with line-number citations, while
`register_peer` had only test callers and no production site ever sent
`NodeRequestType::Register`. A `NodeRegistryConfig::register_direct_links`
function was reported to exist at registry.rs:131 during this investigation —
**it does not exist anywhere in the repo** (verified case-insensitively).

**2. Identities must be pre-generated, and the peer/port matrix centrally computed.**
`identity_path` is a config field (`src/config.rs:105`) and
`NodeIdentity::generate_in_memory()` needs no file I/O, so keystores can be
written before first boot — which is required, because `bootstrap_peers` carries
each peer's hex RNS public key at seed time. Port model: a node with P peers
binds P interfaces on `udp_port + 0 ..= udp_port + P+1`, and node D's interface
for peer P must forward to `base_P + (D's index in P's peer list)` — the **j-rule**
(`tests/pipeline_integration.rs:413-426`). An independent process cannot derive
that locally; the launcher must emit it.

**3. Density is the open question, and two assumed ceilings are false.**
The repo's own e2e test documents that a dense 9-node mesh left "a stable subset
of directed routes never … live", which is why it rides a 5-node chain with ≤3
interfaces (`tests/pipeline_integration.rs:13-26`). **Two constraints claimed by
subagent research during this session are FALSE and must not be designed around:
`max_peers: 25` and `interface_count: 6` do not exist anywhere in rns-net 0.7.0**
(verified by crate-wide grep; `with_transport_enabled` only sets
`transport_enabled`, `config_builder.rs:94-97`). The real ceilings are physical
and empirical: a 32-node full mesh = 31 UDP sockets and ≈70 threads per process
(`WORKER_THREADS=4` + 1 announce worker + 2 threads per interface). Two leaves
can never reach each other through a third leaf (announce retransmit and LRPROOF
forwarding are transport-gated in rns-core), so today every Message-carrying edge
must be a direct link — which is precisely what makes full-mesh density expensive.
Relay (`transport_enabled: true`) is untested in this repo.

**Smaller verified launch blockers:** `/env` and `config.json` are hardcoded
consts (`config.rs:63-64`) — fine for a shared env spec, but the env spec's
single absolute `log_file` interleaves across same-host nodes;
`known_destinations_ttl` is 48 h, so a launcher must wipe stale RNS state between
runs; **peering requires a live route in both directions** (a control frame is
~5.9 KB and cannot ride a direct packet), so a launcher that cannot get announces
to every edge gets nodes that boot, peer with nobody, and log nothing worse than
"no live route yet"; and any shielded transaction makes a node pay `keygen_vk`
**~2.5 min** per process (lazy `SHIELDED_VALIDATOR_VERIFIER`), which caps
same-host shielded testnets until a vk-on-disk cache exists.

Fixed while building the initiator: the composite ignored `rns_port`
(`build.rs:44`) so N composites collided on 4242 and "booted without transport"
silently; and the composite dropped directory responses because its data-plane
branch only knew about pipeline `Message`s.
