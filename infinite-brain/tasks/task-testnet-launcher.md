---
id: task-testnet-launcher
title: "Testnet launcher: key/genesis generator, sparse-topology allocator"
type: task
namespace: pneumatic
visibility: namespace
summary: "PARTIAL. Peering and generation both landed 10/02/2026: nodes register with each other over UDP (fact-control-plane-peering), and testnet-gen emits the keys, configs, env dirs, genesis and peer/port matrix (fact-testnet-generator). Density is answered — pruning topology never lowers the worst node (fact-fanout-graph-density). Remaining: the up/down/status launcher and converting the older seeded tests."
auto_inject: false
applicable_when: "Building or extending local testnet tooling, or debugging why a multi-node cluster does not route"
confidence: 0.85
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "If a launcher/generator exists that writes keystores and emits the peer+port matrix, or if relay (`transport_enabled: true`) is exercised in this repo"
tags: [task, testnet, peering, rns, topology, launcher, transport]
edges:
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.85
    note: "The launcher starts the fleet; the rollout roadmap says what has to be true before that fleet is worth launching"
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

**2. (DONE 10/02/2026 — was: identities had to be pre-generated and the
peer/port matrix centrally computed.)** `testnet-gen/` emits keystores
(through the loader's own writer), per-node `config.json` with both-sided
`bootstrap_peers`, per-node env dirs, and `genesis.json` keyed by Ed25519.
See `fact-testnet-generator`. Historical record of the constraint, because
it is what forced a generator rather than a script:
`identity_path` is a config field (`src/config.rs:105`) and
`NodeIdentity::generate_in_memory()` needs no file I/O, so keystores can be
written before first boot — which is required, because `bootstrap_peers` carries
each peer's hex RNS public key at seed time. Port model: a node with P peers
binds P interfaces on `udp_port + 0 ..= udp_port + P+1`, and node D's interface
for peer P must forward to `base_P + (D's index in P's peer list)` — the **j-rule**
(`tests/pipeline_integration.rs:413-426`). An independent process cannot derive
that locally; the launcher must emit it.

**3. Density: answered 10/02/2026, and the answer is not topology.
Two assumed ceilings remain false and must not be designed around.**
`fact-fanout-graph-density` measures it: the send graph is a full mesh minus
executor↔executor, so pruning saves 5.8% at equal role counts and 50% when
executors dominate, but **never lowers the worst node**, which is the binding
constraint. What is still unknown is empirical, not structural:
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

**4. (PARTLY DONE 10/02/2026 — was: the integration tests could not see the
wiring they depend on.)** `tests/peered_topology_e2e.rs` now builds a 4-node mesh
whose directories are *derived* by peering — nothing calls `register_peer` — and
routes the pipeline's real hops over them. Mutation-verified against the ack
defect. See `fact-integration-test-fixture-blind-spot`. Still open: the three
older multi-node tests (`pipeline_integration`, `shielded_pipeline`,
`transport_integration`) keep seeding with `register_peer(key, rhash, node_type,
conn)`, a test-only API with no production caller, at 6 sites. They stay blind to
registration, ack handling and control framing. Converting them is easier once a
launcher computes the peer/port matrix, since a derived topology has to be
expressible before it can be asserted.

**5. The launcher (`up` / `down` / `status`) is the remaining piece.** Everything it
needs to read already exists in `manifest.json` — every node's directory, base
port, interface count, peer names, all three public-key forms, and since the
multi-host work each node's `address`, the `placement` and `ports_per_host`. What it must
do: start the data service first with `PNEUMATIC_GENESIS` pointed at the generated
`genesis.json` (nodes fail closed without a listener), wipe stale RNS state before
each boot (`known_destinations_ttl` is 48 h, so yesterday's routes poison today's
run), start each node with `PNEUMATIC_CONFIG_FILE`, `PNEUMATIC_ENV_DIR` and
`PNEUMATIC_DATA_ADDR`, then poll until every directory is populated — observable
now, rather than something to eyeball in logs.

**6. Multi-host / cloud deployment (started 10/02/2026).** The hard blocker is
gone: the composite used to bind `127.0.0.1` (the builder's default, never
overridden — `fact-transport-loopback-bind-default`) and so could not be reached
across hosts at all. `testnet-gen` now has per-host placement: peers dial real
addresses (`--addresses` / `--addresses-file`) and every machine shares one UDP
range, which is the firewall consequence of the address doing the disambiguating.
What remains is the deployment plumbing itself, and the constraints that shape it:
leaves cannot route through other leaves, so the mesh needs flat, directly
routable L3 — a NAT gateway or load balancer cannot stand in; peer addresses live
in `bootstrap_peers`, so ephemeral instance IPs invalidate configs fleet-wide and
need statically assigned addresses; and dense mesh has **never been exercised off
loopback** (the suite's own dense test documents dead routes on loopback, where
there is no loss or jitter), so the first milestone is 4 nodes on 4 instances
proving mesh formation, not 40. Kubernetes is a poor fit for the same reason the
port model is rigid. Not yet built: the `ip_address`-based bind is loadable but
the launcher must decide per host; key custody is still "the operator holds every
validator key", acceptable for a testnet and worth keeping deliberate.

**Smaller verified launch blockers:** `/env` and `config.json` were hardcoded
consts (`config.rs:63-64`; now overridable via `PNEUMATIC_ENV_DIR` /
`PNEUMATIC_CONFIG_FILE` — see `fact-testnet-generator`) — fine for a shared env
spec, but the env spec's single absolute `log_file` interleaves across same-host
nodes (the generator rewrites it per node);
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
