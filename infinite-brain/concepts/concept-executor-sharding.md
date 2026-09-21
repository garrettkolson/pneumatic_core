---
id: concept-executor-sharding
title: "Executor sharding per epoch (Shuffler)"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Executors are sharded per epoch by the Shuffler: a per-epoch assignment of executor sets determines who proposes/attests, rotated for fairness and liveness."
auto_inject: false
applicable_when: "Working on epoch.rs shuffling, proposer selection, or executor assignment"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the Shuffler algorithm, sharding granularity, or epoch rotation changes"
tags: [epoch, sharding, executors, shuffle]
edges:
  - target: concept-optimistic-finality
    type: related_to
    weight: 0.8
    note: "Sharded executors are the signers optimistic finality relies on"
  - target: source-protocol-changes
    type: derived_from
    weight: 0.9
    note: "Phase-0 decision: same StakeSet for election and shuffling"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.8
    note: "Lattice block production is sharded per epoch"
related: []
source_url: "Empty"
---

# Executor sharding per epoch (Shuffler)

Block production is **sharded across executors per epoch**: the `Shuffler` (in `src/epoch.rs`, alongside `Epoch`, `StakeSet`, `ExecutorSet`, `LeaderSelector`, `BlockProposer`) deterministically reshuffles the executor set each epoch, so proposal/attestation duty rotates.

Per the Phase-0 decisions, leader election for a token and conflict voting draw on the **same StakeSet** — one staking state, two uses — which keeps the shuffling verifiable and consistent across the roles. This is what keeps a large executor fleet tractable: any given (token, slot) has a known, small responsible set.
