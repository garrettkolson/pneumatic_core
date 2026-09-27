---
id: decision-deterministic-per-tx-routing
title: "ADR-004: Deterministic per-transaction routing (not epoch-wide leader)"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Each transaction routes to its own finalizer via deterministic_select(stakers, tx_id, epoch) over a stake snapshot frozen at the epoch boundary — the epoch-wide leader is a throughput bottleneck; the frozen snapshot eliminates state divergence."
auto_inject: false
applicable_when: "Changing finalizer assignment, the selection function, or snapshot handling"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when routing reverts to an epoch-wide leader or the seed/snapshot inputs change"
tags: [adr, design-decision, routing, determinism, throughput]
edges:
  - target: pattern-two-tier-snapshot-cache
    type: supports
    weight: 0.8
    note: "The tiered cache is the latency machinery this decision relies on"
  - target: concept-sentinel-role
    type: supports
    weight: 0.8
    note: "The sentinel is where per-tx assignment runs (assign_finalizer_deterministic)"
  - target: decision-deterministic-leader-election
    type: related_to
    weight: 0.7
    note: "Same seeded-SHA-256-sorted-walk algorithm; the seed is the tx id, not the epoch"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated verbatim from README ADR-004 when the README was trimmed (09/26/2026)"
related: []
source_url: "repo:README.md (former lines 122-126)"
---

# ADR-004: Deterministic per-transaction routing (not epoch-wide leader)

Each transaction is routed to a specific finalizer via `deterministic_select(stakers, seed_bytes, epoch_number)` where `seed_bytes = tx_id_bytes`. A stake snapshot frozen at the epoch boundary is the selection authority.

**Rationale**: An epoch-wide leader is a throughput bottleneck — only one node proposes per epoch. Per-transaction routing distributes work across all staked nodes. The stake snapshot eliminates state divergence: all nodes agree on the selection authority because it's persisted in `DataProvider` at epoch boundaries. The tiered cache (local → DataProvider → peer) minimizes network latency for the common case (local cache hit).
