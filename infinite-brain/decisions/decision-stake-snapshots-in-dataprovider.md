---
id: decision-stake-snapshots-in-dataprovider
title: "ADR-006: Stake snapshots persisted in DataProvider, not blocks"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Epoch-boundary StakeSet/ExecutorSet snapshots are stored in the DataProvider (the shared state layer), not embedded in block headers — avoids block bloat and keeps chain state independent of stake topology."
auto_inject: false
applicable_when: "Changing where epoch snapshots live, block header contents, or snapshot recovery"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when snapshots move into block headers or the DataProvider snapshot methods are replaced"
tags: [adr, design-decision, stake-snapshot, data-provider, epochs]
edges:
  - target: concept-data-provider
    type: supports
    weight: 0.9
    note: "get_stake_snapshot / save_stake_snapshot are the persistence seam"
  - target: pattern-two-tier-snapshot-cache
    type: related_to
    weight: 0.7
    note: "The tiered cache reads from this persistence layer on miss"
  - target: concept-epoch-and-staking
    type: related_to
    weight: 0.7
    note: "Snapshots are the frozen epoch state the decision governs"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated verbatim from README ADR-006 when the README was trimmed (09/26/2026)"
related: []
source_url: "repo:README.md (former lines 134-138)"
---

# ADR-006: Stake snapshots persisted in DataProvider (not blocks)

Stake snapshots are stored in `DataProvider` (an abstracted external store), not embedded in block headers.

**Rationale**: Embedding snapshots in blocks would bloat block size and make chain state dependent on stake topology. DataProvider is already the shared state layer between nodes — the natural place for epoch-level state. Nodes recover snapshots on demand from the sentinel's cache hierarchy, with the primary tier a local in-memory cache populated from the first block of a new epoch.
