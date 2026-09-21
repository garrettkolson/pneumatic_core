---
id: concept-shielded-pool
title: "Shielded pool — canonical note universe"
type: concept
namespace: pneumatic
visibility: namespace
summary: "The global pool that accumulates all shielded note commitments; source of the Merkle roots the validation spec checks. Read-only view exists; real pool comes in S5.4."
auto_inject: false
applicable_when: "Working on pool state, Merkle roots, S5.3/S5.4, or balance lookups"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the real ShieldedPool is wired in (S5.4) or the pool semantics change"
tags: [shielded, pool, notes, state]
edges:
  - target: decision-global-shielded-pool
    type: derived_from
    weight: 1.0
    note: "The decision this concept instantiates"
  - target: concept-note-commitment
    type: depends_on
    weight: 1.0
    note: "The pool is a set of note commitments"
  - target: concept-incremental-merkle-tree
    type: depends_on
    weight: 1.0
    note: "Pool state is tracked via its incremental Merkle tree"
  - target: concept-shielded-pool-view
    type: supports
    weight: 0.9
    note: "The read-only seam standing in for the pool until S5.4"
related: []
source_url: "Empty"
---

# Shielded pool — canonical note universe

The shielded pool is the **global, consensus-tracked collection of all shielded note commitments** ever issued. It is the canonical note universe: when a shielded transaction commits, its output note commitments are appended to the pool, and the pool's Merkle root is what validators compare against (freshness + recency window).

Lifecycle: **S5.1** introduced the read-only `ShieldedPoolView` seam so the validation spec could be built before the pool existed; **S5.3** lands the append path; **S5.4** swaps `SimpleShieldedPoolView` for the real `Arc<ShieldedPool>`. Until then, tests and the validation path run against the simple view.
