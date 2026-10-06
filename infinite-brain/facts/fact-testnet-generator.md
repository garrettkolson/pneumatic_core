---
id: fact-testnet-generator
title: "testnet-gen: generated keystores, configs, and the two-sided port matrix"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/02/2026: `testnet-gen/` emits a bootable multi-node testnet — keystores via the loader's own writer, per-node config.json with both-sided bootstrap_peers, per-node env dirs, genesis keyed by Ed25519, and a manifest. New core overrides PNEUMATIC_CONFIG_FILE / PNEUMATIC_ENV_DIR make N nodes coexist on one host; a test drives a generated node through the real Config::build()."
auto_inject: true
applicable_when: "Spinning up a local cluster, changing what a node's config must contain, or extending the launcher"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "If ConfigSpec gains a required field, if the keystore format changes, if the j-rule port model changes, or if a launcher script supersedes the printed boot instructions"
tags: [fact, testnet, generator, config, keystore, rns, ports, tooling]
edges:
  - target: fact-fanout-graph-density
    type: depends_on
    weight: 0.9
    note: "The role-graph topology is the fan-out graph; the cost table reports its measured density"
  - target: concept-env-driven-config
    type: depends_on
    weight: 0.85
    note: "Emits config.json + an env dir per node, and the path overrides are what make per-node env dirs reachable"
  - target: fact-data-service
    type: related_to
    weight: 0.7
    note: "Genesis it writes is applied by the data service; a node still fails closed without one listening"
  - target: fact-register-ack-bucket-placement-defect
    type: related_to
    weight: 0.6
    note: "Both exist to make the wrong-key / wrong-bucket classes unreachable by construction rather than by operator care"
  - target: task-testnet-launcher
    type: supports
    weight: 0.95
    note: "Closes the pre-generated-keys + centrally computed peer/port matrix blocker"
related: ["[[Environment-driven configuration: config.json + /env/ specs]]"]
source_url: "testnet-gen/"
---

# testnet-gen: generated keystores, configs, and the two-sided port matrix

Landed 10/02/2026. `cargo run -p pneumatic_testnet_gen --bin pneumatic_testnet_gen
-- --out <dir> [--validators N | --sentinels/--executors/--finalizers/--committers]
[--topology full-mesh|role-graph|auto]`. 40 validators in **2.5 s**.

**Why a crate and not a script.** Two things in the config cannot be produced
correctly outside the codebase:

1. **Keystores.** `bootstrap_peers` carries each peer's hex RNS public key at seed
   time, so every identity must exist before first boot. Written with
   `NodeIdentity::load_or_create` — the loader's own writer — so the hybrid format
   (Ed25519 seed + ML-DSA + ML-KEM keypairs, mode 0600) cannot drift, and reload
   reconstructs the same keys instead of regenerating them. Re-running **preserves**
   keys (replacing one orphans the genesis stake under the old key); `--force-keys`
   is the explicit escape hatch.
2. **The j-rule.** Interface `k` on a node listens on `base + k` and must forward
   to `peer_base + j`, `j` being *this* node's index in *that peer's* peer list.
   `j` is a property of another node's config, so no node can derive it locally.

**What it writes.** `nodes/<name>/{node_identity.json, config.json, env/env.json}`,
`genesis.json`, `manifest.json`. The env dir is the template with `log_file`
repointed per node (one absolute path shared by N nodes interleaves into mush);
`shielded_root_recency` is **copied from the env template into genesis**, so the
coupling the runbook warns about is enforced rather than remembered.

**Multi-host placement (10/02/2026).** `Placement::SingleHost` (default: peers dial
loopback, port windows disjoint) versus `Placement::PerHost { addresses }`: one
node per machine, peers dial real addresses, and **every host shares one port
range** — the address disambiguates, so a 40-node fleet needs one UDP range per
instance, not one per node. `--addresses a,b,c` or `--addresses-file` (one per
line, `#` comments allowed, so a provisioner's output feeds straight in). Supplying
addresses *is* the declaration of per-host placement; there is no separate flag
that could contradict the list. A count mismatch fails the build rather than
padding with loopback. `--bind-ip` emits the new `ip_address` key (omitted when
unset, so "bind every interface" stays the absence of a decision rather than an
invented value).

The two-pass split that cloud provisioning needs — identities first, then configs
listing peers' public keys — was already there for ordering reasons. Also fixed:
`--validators n` divided by four and dropped the remainder, so `--validators 10`
silently built 8 nodes and `--validators 1..3` failed as "every role count is
zero". It now distributes the remainder in role order and prints the split.

**Two failure classes it exists to make unreachable.** Genesis is keyed by each
node's **Ed25519** key while `bootstrap_peers` carries its **RNS** key — both
64-byte hex in the same manifest, and swapping them yields a node that boots,
installs no roles, and rejects registrations with no error naming the cause. And
a reversed j-rule yields a cluster where every config looks populated, every node
boots, and no link ever handshakes. Both are pinned by tests: `genesis_is_keyed_by_
ed25519_keys_and_never_by_rns_keys`, and `bootstrap_peer_ports_are_the_targets_
listen_ports` — which was written *after* the emitter had the direction backwards
(it computed the port the peer uses to reach us and wrote it into our config), and
which fails that mutant with `sentinel-1 must forward to sentinel-2 on base 21009 + j 0`.

**Core change it needed.** `/env` (absolute) and `config.json` (CWD-relative) were
hardcoded, which is right in a container and unusable for N processes on one host.
`PNEUMATIC_CONFIG_FILE` / `PNEUMATIC_ENV_DIR` now override them; unset or blank
behaves exactly as before, and a bad override fails at the same `fs::read` with the
same error rather than silently defaulting.

**Verification.** `tests/boot_config.rs` takes a generated node through the real
`Config::build()` — config, env dir, keystore — and asserts the loaded identity is
the one genesis was written for. Plus 11 artifact tests against `ConfigSpec`,
`EnvironmentMetadata::load_from_spec`, and `NodeIdentity::load_or_create`, 12
topology/placement unit tests, and 4 tests that invoke the **binary** (flag
semantics live in `main.rs`, unreachable from the library). **1101 passed /
37 ignored / 0 failed** workspace-wide after this (see `fact-test-suite-cloud`).
Two mutation-verified: reverting the j-rule direction fails
`bootstrap_peer_ports_are_the_targets_listen_ports`, and replacing the peer's
address with a shared constant fails `per_host_placement_writes_each_peers_own_address`
— the test that exists because a single-host cluster cannot distinguish "correct"
from "everyone dials 127.0.0.1". Generated trees are gitignored: they hold private keys.
