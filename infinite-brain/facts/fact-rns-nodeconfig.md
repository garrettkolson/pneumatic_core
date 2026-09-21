---
id: fact-rns-nodeconfig
title: "RNS NodeConfig: ~45 fields, no Default — RnsNodeConfigBuilder is the single choke point"
type: fact
namespace: pneumatic
visibility: namespace
summary: "rns-net 0.7.0 NodeConfig has ~45 fields and no Default; the full literal lives at config_builder.rs:120-172 (UDP 4242, per-peer udp_port+i, no TCP in v1, 48h dest TTL)."
auto_inject: false
applicable_when: "Bumping rns-net, changing RNS listen/peer topology, or debugging RNS startup behavior"
confidence: 0.95
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the NodeConfig literal in src/rns/config_builder.rs changes, or rns-net's version/NodeConfig shape changes"
tags: [rns, nodeconfig, builder, udp, config]
edges:
  - target: fact-rns-pinning
    type: derived_from
    weight: 1.0
    note: "The ~45-field no-Default constraint comes from the pinned rns-net 0.7.0"
  - target: concept-rns-transport
    type: supports
    weight: 0.9
    note: "RnsNetwork is built from this single NodeConfig"
  - target: pattern-pinned-dependencies
    type: related_to
    weight: 0.7
    note: "Exact rns pins are what make the one-place config strategy viable"
related: []
source_url: "Empty"
---

# RNS NodeConfig: ~45 fields, no Default, one builder

Because the pinned rns-net 0.7.0 `NodeConfig` has **~45 fields and no `Default` impl** (src/rns/mod.rs:4-6), there is exactly one place the full config literal is written: `RnsNodeConfigBuilder::build()` at **src/rns/config_builder.rs:120-172**. The module docs call this "the single choke point for rns-net API churn" — a version bump touches one function.

Verified defaults/behavior of the builder (src/rns/config_builder.rs):

- `listen_ip` defaults to `"127.0.0.1"`; `transport_enabled` defaults to `false` (opt-in).
- `DEFAULT_UDP_PORT = 4242` (config_builder.rs:27).
- Peers are wired as **point-to-point UDP interfaces, one per peer**, on `udp_port + i` (config_builder.rs:120-172 region) — no shared multi-peer socket in v1.
- **No TCP interfaces** in v1; no multicast.
- `KNOWN_DESTINATIONS_TTL` = 48 hours.

`NodeIdentity` (src/rns/identity.rs) supplies the transport keypair and rhash that the config's destination registry builds from, so config and identity are co-located in `src/rns/`.
