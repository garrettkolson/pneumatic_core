---
id: decision-executor-sharding
title: "ADR-009: Executor sharding with per-epoch rotation"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Executors are partitioned into disjoint per-epoch shards via a SHA-256-seeded Fisher-Yates shuffle + stake-balanced round-robin; each tx routes only to its shard, and per-epoch reshuffling prevents stable cartel formation."
auto_inject: false
applicable_when: "Changing shard assignment, shard count, executor rotation, or shard-aware routing"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when sharding is removed, the partition algorithm changes, or rotation stops being per-epoch"
tags: [adr, design-decision, sharding, executor, throughput]
edges:
  - target: concept-executor-sharding
    type: supports
    weight: 0.95
    note: "deterministic_select_shard + Shuffler implement this decision"
  - target: concept-executor-role
    type: related_to
    weight: 0.7
    note: "Sharding decides which executor instances serve a given transaction"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated verbatim from README ADR-009 when the README was trimmed (09/26/2026)"
related: []
source_url: "repo:README.md (former lines 162-166)"
---

# ADR-009: Executor sharding with per-epoch rotation

Executors are partitioned into disjoint shards per epoch. `deterministic_select_shard(executors, shard_count, tx_id, epoch_number)` uses a SHA-256 seeded Fisher-Yates shuffle followed by stake-balanced round-robin partitioning. Each transaction is routed only to the executors in its assigned shard.

**Rationale**: Broadcasting every transaction to all executors creates a linear bottleneck — throughput is bounded by the slowest executor in the entire set. Sharding reduces per-executor load proportionally to `1/shard_count`. Stake-balanced round-robin prevents one shard from accumulating disproportionate stake (which would lower its effective quorum and create a single point of failure). Per-epoch rotation via `advance_epoch()` reshuffles executor-to-shard assignments, preventing stable cartel formation among bad actors. The `ExecutorSetCache` and `StakeSnapshotCache` are invalidated together on epoch boundary, ensuring a consistent view.
