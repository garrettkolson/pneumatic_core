---
id: concept-env-driven-config
title: "Environment-driven configuration: config.json + /env/ specs"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Config::build loads config.json plus every JSON spec in /env/ into per-environment EnvironmentMetadata (partitions, quorum, crypto, cost model, validators); invalid specs fail boot."
auto_inject: false
applicable_when: "Changing node boot, environment specs, cost model, quorum/shard parameters, or validation spec registration"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If config.json//env file layout, EnvironmentMetadata fields, or spec validation-on-load changes"
tags: [config, environment, cost-model, quorum]
edges:
  - target: fact-workspace-layout
    type: related_to
    weight: 0.6
    note: "config.json + /env/ directory are the node's filesystem contract"
  - target: concept-node-registry
    type: related_to
    weight: 0.8
    note: "node_type/node_registry_types + per-type NodeTypeConfig (min/max/min_stake) shape registry admission"
  - target: concept-validation-specs
    type: related_to
    weight: 0.85
    note: "EnvironmentMetadata carries both spec registries, populated from spec name lists"
  - target: concept-executor-sharding
    type: related_to
    weight: 0.6
    note: "shard_count / shard_quorum_percentage are environment-level fields (environment.rs:157-160)"
related: []
source_url: "Empty"
---

# Environment-driven configuration: config.json + /env/ specs

`Config::build` (`src/config.rs:66`) is two-phase: `load_spec` reads `config.json` (lines 63, 148) for node identity, `node_type` (Full/Light), and transport settings; `get_environment_metadata` (lines 156-211) reads **every file in `/env/`** (line 64), parses each as an `EnvironmentMetadataSpec`, runs `env_spec.validate()` (line 186) and `load_from_spec` (line 197), and indexes results by `environment_id` in a DashMap. A spec whose quorum/risk/tax/gas/shard values are out of range fails node boot, instead of silently neutering the risk gate or finalization quorum.

`Config` (config.rs:32) is identity-authoritative: `public_key` always comes from the persistent identity (`identity.ed25519`), and any `public_key` in `config.json` is deliberately ignored (lines 35-40). It also carries per-type `NodeTypeConfig` (min/max connections + minimum stake, lines 213-225, 265-279) and the Full/Light registry-type split (line 246-257: light nodes skip Archiver).

`EnvironmentMetadata` (`src/environment.rs:127`) is the per-environment policy object: partition ids (token/contract/proxy-auth/slush), `quorum_percentage`/`override_quorum_percentage`/`shard_quorum_percentage`, `max_risk`, the `CostModel`, an `AsymCryptoProvider`, both validation-spec registries, allowed token types, logger, and shard count. `CostModel` (line 15) prices actions via fixed-point gas (`gas = base_cost + amount × multiplier`, integer scale 10 000, lines 56-93) and holds staking policy: `global_min_stake`, per-type stake overrides, admin tax, and `slash_fraction`.
