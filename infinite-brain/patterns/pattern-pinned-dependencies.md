---
id: pattern-pinned-dependencies
title: "Exact-pinned external dependencies"
type: pattern
namespace: pneumatic
visibility: namespace
summary: "Security-sensitive externals (rns-net/rns-crypto/rns-core) are =-pinned in the workspace Cargo.toml; a single choke-point builder keeps consensus behavior from drifting."
auto_inject: false
applicable_when: "Adding/upgrading dependencies, touching rns, or reviewing Cargo.toml changes"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If new dependencies are added with floating versions, or rns unpins"
tags: [dependencies, pinning, supply-chain, pattern]
edges:
  - target: fact-rns-pinning
    type: supports
    weight: 1.0
    note: "The concrete pinned set"
  - target: fact-hybrid-pq-crypto
    type: related_to
    weight: 0.6
    note: "pqcrypto versions ride the same regime"
related: []
source_url: "Empty"
---

# Exact-pinned external dependencies

Consensus- and transport-relevant external crates are **pinned to exact versions** in the workspace `Cargo.toml` (`rns-net = "=0.7.0"`, `rns-crypto = "=0.1.9"`, `rns-core = "=0.1.16"`). Rationale: rns APIs churn between minor versions (e.g. 0.7.0's `NodeConfig` has ~45 fields and no `Default`), and a floating version could silently change consensus-adjacent behavior on the next `cargo update`.

The companion discipline is a **single choke point** for each pinned API's awkwardness: `RnsNodeConfigBuilder` (`src/rns/config_builder.rs`) is the one place that knows rns-net 0.7.0's full `NodeConfig`, so an upstream bump is a one-file migration. Apply the same shape when adding a new pinned dependency.
