---
id: decision-conflict-definition-and-resolution
title: "ADR-008: Conflict = same-parent siblings; stake wins, same-proposer slashes"
type: decision
namespace: pneumatic
visibility: namespace
summary: "A conflict is two valid Blocks with the same (token_id, previous_hash) and different current_hash; higher stake wins (hash tie-break), a different-proposer loser is discarded, and a same-proposer double-sign slashes both."
auto_inject: false
applicable_when: "Changing conflict detection, resolve_block_conflict outcomes, or slashing policy"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the conflict definition, resolution ordering, or the proposer-identity branch changes"
tags: [adr, design-decision, conflict, slashing, consensus]
edges:
  - target: concept-candidate-registry-conflict
    type: supports
    weight: 0.95
    note: "CandidateRegistry + resolve_block_conflict implement exactly this definition"
  - target: concept-optimistic-finality
    type: related_to
    weight: 0.8
    note: "Conflicts are the dispute case the optimistic path defers to the quorum machinery"
  - target: source-protocol-changes
    type: derived_from
    weight: 0.8
    note: "Phase-0 decision 1/4 (conflict definition) resolved here"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated from README ADR-008 when the README was trimmed (09/26/2026)"
related: []
source_url: "repo:README.md (former lines 146-160)"
---

# ADR-008: Conflict detection and resolution strategy

A **conflict** is defined as two valid `Block`s that reference the same `(token_id, previous_hash)` but produce different `current_hash` values — two competing proposals for the same chain position.

Detection: `CandidateRegistry` — a DashMap keyed by `(token_id, previous_hash)` collecting all candidate blocks at each position. When `insert()` appends a second block with a different hash, `has_conflict()` returns true. This works for all conflict scenarios (same-parent race, close-together commits, double-spends) because they all manifest as sibling blocks at the same chain position.

Resolution: `resolve_block_conflict()` — higher-stake proposer wins; tie-break by lexicographic hash comparison (smaller hash wins). The response then branches on proposer identity:

| Winner stake > Loser stake | Same proposer? | Response |
|---|---|---|
| Yes | No | Discard the loser (network race) |
| Yes | Yes | **Slash both** (double-signed block is a protocol violation — higher stake doesn't excuse the proposer) |
| Tied | Any | Tie-break winner; flag both proposers for review |

**Rationale**: Defining conflict at the chain-position level (same `previous_hash`) rather than by provenance keeps detection simple and correct across all scenarios — a race, a double-spend, and a malicious double-propose all look identical at the chain level. The branch on proposer identity is the *only* provenance check needed: it distinguishes an honest relaying race (different proposers) from an intentional violation (same proposer).
