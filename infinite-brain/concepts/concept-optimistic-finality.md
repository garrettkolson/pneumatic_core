---
id: concept-optimistic-finality
title: "Optimistic finality (first executor sig finalizes)"
type: concept
namespace: pneumatic
visibility: namespace
summary: "2/3 quorum voting is replaced by optimistic commit: the first valid executor signature finalizes a block (Finalizer::try_finalize_optimistic); conflicts resolved via same-StakeSet voting."
auto_inject: false
applicable_when: "Touching finalizer logic, conflict handling, or finality guarantees"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If finality reverts to quorum-based voting"
tags: [finality, optimistic, finalizer, protocol]
edges:
  - target: source-protocol-changes
    type: derived_from
    weight: 1.0
    note: "Phase-0 decision 2/4 resolved here"
  - target: pillar-block-lattice
    type: part_of
    weight: 1.0
    note: "The finality mechanism of the lattice"
  - target: concept-candidate-registry-conflict
    type: supports
    weight: 0.9
    note: "Conflicts are detected and routed through the candidate registry"
  - target: event-s5-2-finalizer-wiring
    type: related_to
    weight: 0.8
    note: "Finalizer is the role where shielded wiring landed"
related: []
source_url: "Empty"
---

# Optimistic finality

PROTOCOL_CHANGES.md (all four Phase-0 decisions resolved) replaces **2/3 quorum voting** with **optimistic commit**: the **first valid executor signature** on a block finalizes it — `Finalizer::try_finalize_optimistic`. A block is committed optimistically; disputes are handled after the fact.

Conflict definition: **two valid blocks with the same `(token_id, previous_hash)`**. Resolution uses the **same StakeSet** for leader election and conflict voting; conflicting blocks are **discarded**, and slashing applies only on **double-sign** by the proposer (`ConflictResolution::SameProposerSlash`) — not merely for competing blocks. This keeps finality fast (no quorum round-trip) while retaining a deterministic penalty for genuine misbehavior.
