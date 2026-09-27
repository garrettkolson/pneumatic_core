---
id: decision-optimistic-finality
title: "ADR-005/010: Optimistic commit with quorum as dispute mechanism"
type: decision
namespace: pneumatic
visibility: namespace
summary: "The first valid executor signature triggers immediate block finalization; the 2/3 quorum machinery is repurposed for conflict resolution only — blocks go Optimistic → Confirmed via stake-weighted quorum gossip."
auto_inject: false
applicable_when: "Touching finality guarantees, the quorum gate, or the Optimistic/Confirmed state"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale if finality reverts to happy-path quorum voting (see concept-optimistic-finality)"
tags: [adr, design-decision, finality, optimistic, quorum]
edges:
  - target: concept-optimistic-finality
    type: supports
    weight: 0.95
    note: "The implemented mechanism: try_finalize_optimistic on the first authenticated signature"
  - target: concept-quorum-gossip-protocol
    type: supports
    weight: 0.9
    note: "The quorum gossip is the dispute/confirmation tail this decision repurposes"
  - target: concept-candidate-registry-conflict
    type: related_to
    weight: 0.8
    note: "Conflicts are the only case that re-engages the quorum machinery"
  - target: source-protocol-changes
    type: derived_from
    weight: 0.8
    note: "Phase-0 decision 2/4 resolved optimistic finality; README ADR-005/010 recorded it"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated from README ADR-005 + ADR-010 (two entries, one decision) when the README was trimmed (09/26/2026)"
related: []
source_url: "repo:README.md (former lines 128-132, 168-172)"
---

# ADR-005 / ADR-010: Optimistic commit with quorum as dispute mechanism

Standard tokens commit immediately after single-executor execution + single-finalizer signature. The 2/3 quorum machinery is repurposed for conflict resolution only — invoked when `CandidateRegistry` detects a genuine fork (two proposers building on the same parent).

**Rationale**: In the vast majority of cases no fork occurs. Requiring 2/3 quorum in the happy path wastes bandwidth and latency; one honest executor's signature is sufficient proof. The quorum protocol remains available to resolve genuine conflicts, where its safety guarantees matter. Blocks start as `Optimistic` and upgrade to `Confirmed` once stake-weighted quorum is reached — the Committer accumulates `voting_stake` per unique voter against `total_stake × quorum %` and broadcasts `BlockQuorumReached`. The optimistic path uses a dedicated `try_finalize_optimistic()` code path that bypasses signature reconciliation.

*Note: the former README recorded this decision twice (ADR-005 and ADR-010); it is one decision here.*
