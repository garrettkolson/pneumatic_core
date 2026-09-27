---
id: decision-sentinel-routing-authority
title: "ADR-007: Sentinel as routing authority, not a consensus node"
type: decision
namespace: pneumatic
visibility: namespace
summary: "The sentinel performs deterministic finalizer assignment but does not participate in consensus — no chain state, no voting on block validity; its only state is the cheap, recoverable stake-snapshot cache."
auto_inject: false
applicable_when: "Scoping sentinel responsibilities, adding sentinel state, or scaling sentinels"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the sentinel gains chain state, votes, or any consensus-critical responsibility"
tags: [adr, design-decision, sentinel, routing, separation-of-concerns]
edges:
  - target: concept-sentinel-role
    type: supports
    weight: 0.9
    note: "The implemented role: gatekeeper with routing authority, no consensus duties"
  - target: pillar-block-lattice
    type: related_to
    weight: 0.6
    note: "Separates the routing layer from the consensus lattice"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated verbatim from README ADR-007 when the README was trimmed (09/26/2026)"
related: []
source_url: "repo:README.md (former lines 140-144)"
---

# ADR-007: Sentinel as the routing authority, not a consensus node

The sentinel performs deterministic finalizer assignment but does not participate in consensus. It does not store chain state or vote on block validity.

**Rationale**: The sentinel is a routing proxy, not a consensus participant. This separation of concerns means the sentinel can be replaced or scaled independently of the consensus protocol. The sentinel's only state is the stake snapshot cache, which is cheap to maintain and easy to recover.
