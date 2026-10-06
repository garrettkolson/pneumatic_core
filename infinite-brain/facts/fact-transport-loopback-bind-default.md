---
id: fact-transport-loopback-bind-default
title: "RNS binds 127.0.0.1 by default, and node-server never overrode it"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/02/2026: `RnsNodeConfigBuilder::new()` defaults `listen_ip` to 127.0.0.1 (config_builder.rs:57). The committer translated `Config.ip_address` into a bind address; node-server applied none, so the composite silently bound loopback and was unreachable from any other host. Fixed by `Config::rns_listen_ip` in core (shared) plus a loadable `ip_address` in config.json."
auto_inject: true
applicable_when: "Deploying more than one node per host or network, changing transport bind behavior, or debugging a cluster that boots but peers with nobody"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "If RnsNodeConfigBuilder's default listen_ip changes, if a binary sets with_listen_ip from something other than Config::rns_listen_ip, or if ip_address gains a non-Config source"
tags: [fact, transport, rns, bind, listen-ip, deployment, defect, config]
edges:
  - target: concept-rns-transport
    type: depends_on
    weight: 0.9
    note: "The interface model is why a wrong bind is invisible locally and fatal remotely"
  - target: concept-env-driven-config
    type: related_to
    weight: 0.8
    note: "ip_address is a new ConfigSpec field; it was previously hardcoded to unspecified in Config::build"
  - target: fact-testnet-generator
    type: supports
    weight: 0.85
    note: "Multi-host placement is only usable once the composite binds a reachable address"
  - target: task-testnet-launcher
    type: supports
    weight: 0.8
    note: "This was the hard blocker for deploying beyond one host"
related: ["[[RNS transport: the production inter-node wire (RnsNetwork over rns-net, Resource transfer for large payloads)]]"]
source_url: "Empty"
---

# RNS binds 127.0.0.1 by default, and node-server never overrode it

Found 10/02/2026 while scoping cloud deployment, from three greps rather than
from speculation.

```
src/rns/config_builder.rs:57   listen_ip: "127.0.0.1"          ← builder default
committer/src/main.rs          .with_listen_ip(rns_listen_ip(&config))   ✓
node-server/…/build.rs         with_udp_port, add_peer, … no with_listen_ip  ✗
```

The composite therefore bound **loopback only**: it booted, announced, loaded its
identity, and was unreachable from every other host. Nothing caught it because
every test in the repo is a single-host mesh, where a loopback bind is
indistinguishable from a correct one. The failure signature is the same family as
the j-rule and wrong-key defects — *a node that looks healthy and peers with
nobody*, with no log worse than "no live route yet".

Compounding it, `Config.ip_address` was **not loadable at all**: `Config::build`
hardcoded `Ipv6Addr::UNSPECIFIED` (config.rs:132), so there was no way to express
"bind this interface" from config.json. The committer's local `rns_listen_ip`
helper did have a configured-address branch, which was dead code.

**The fix, and why it is shaped this way.**

- `Config::rns_listen_ip()` in core is now the single source of truth. The rule
  was duplicated (one binary had it, the other had none); colocating it with
  `Config` is what keeps the two binaries from drifting again — the same failure
  pattern as the two `send_frame` size rules.
- `ConfigSpec.ip_address: Option<String>` (additive, defaulted). Unset →
  unspecified → `0.0.0.0`. A malformed value **stops boot** quoting the bad
  value, because falling back to "any interface" would restore exactly the
  invisible-failure shape above. Blank is treated as unset, matching the
  `PNEUMATIC_CONFIG_FILE` rule.
- Unspecified IPv6 still resolves to `0.0.0.0`: the transport's addresses are IPv4
  in this codebase, so widening to `::` would change wire behavior, not just the
  bind.

**Behavior change to be aware of.** The composite now binds all interfaces where
it previously bound loopback. On a developer machine that means a UDP listener
reachable on the LAN — consistent with what the committer already did, and
narrowable via `ip_address`. Single-host tests are unaffected (binding "any" is a
superset of binding loopback); the suite passed unchanged at 1101/37/0.

For deployment this is the prerequisite: multi-host placement is unusable until
the composite binds a reachable address, and on a multi-homed instance binding
"any" answers on the wrong network.
