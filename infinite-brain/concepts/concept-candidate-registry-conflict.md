---
id: concept-candidate-registry-conflict
title: "CandidateRegistry — conflict detection & resolution state"
type: concept
namespace: pneumatic
visibility: namespace
summary: "CandidateRegistry is keyed by (token_id, previous_hash): the registry a second valid block for the same slot lands in, triggering conflict detection and the discard/slash path."
auto_inject: false
applicable_when: "Working on conflict handling, block routing, or slashing logic"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the registry key changes, or conflict resolution moves to a different structure"
tags: [conflict, registry, blocks, slashing]
edges:
  - target: concept-optimistic-finality
    type: supports
    weight: 1.0
    note: "The after-the-fact dispute mechanism for optimistic commit"
  - target: source-protocol-changes
    type: derived_from
    weight: 1.0
    note: "Phase-0 decision on conflict = same (token_id, previous_hash)"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.9
    note: "Conflict state is per-token-slot in the lattice"
related: []
source_url: "Empty"
---

# CandidateRegistry — conflict state

A block that is *valid* but competes with an already-finalized block for the same slot — i.e. shares its **`(token_id, previous_hash)`** key — is not silently dropped; it lands in the **`CandidateRegistry`** (`src/epoch.rs`), keyed by exactly that pair.

This is the detection seam for the optimistic model: the registry accumulates candidates, conflict voting (on the shared StakeSet) runs against them, losers are **discarded**, and the proposer is **slashed only on double-sign** (`ConflictResolution::SameProposerSlash`). Separating the registry from the main chain state keeps the hot path (optimistic commit) free of dispute logic.
