---
id: fact-rns-pinning
title: "RNS transport: rns-net =0.7.0, rns-crypto =0.1.9, rns-core =0.1.16 (exact pins)"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Reticulum (RNS) transport pinned to exact versions in workspace Cargo.toml; NodeConfig (~45 fields, no Default) built via RnsNodeConfigBuilder; e2e tests over RNS exist."
auto_inject: false
applicable_when: "Upgrading rns crates, touching src/rns/, or diagnosing transport behavior"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the =-pinned rns versions in the workspace Cargo.toml change"
tags: [rns, reticulum, dependencies, transport]
edges:
  - target: pattern-pinned-dependencies
    type: supports
    weight: 1.0
    note: "The concrete instance of the pinning pattern"
  - target: event-rns-e2e
    type: derived_from
    weight: 0.9
    note: "E2E tests landed with the transport"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.8
    note: "RNS inherits the 16 MB MAX_FRAME_SIZE cap"
related: []
source_url: "Empty"
---

# RNS transport: exact version pins

The Reticulum Network Stack transport (`src/rns/`: `conn`, `config_builder`, `identity`, `wrapper`) depends on crates pinned **exactly** in the workspace `Cargo.toml`:

- `rns-net = "=0.7.0"`
- `rns-crypto = "=0.1.9"`
- `rns-core = "=0.1.16"`
- (PQ side: `pqcrypto-mldsa 0.1.2`, `pqcrypto-mlkem 0.1.1`, `pqcrypto-traits 0.3`)

rns-net 0.7.0's `NodeConfig` has ~45 fields and **no `Default`**, so the full config is built in one place — `RnsNodeConfigBuilder` (`src/rns/config_builder.rs`) is "the single choke point for rns-net API churn". End-to-end tests run over RNS (see the e2e event node).
