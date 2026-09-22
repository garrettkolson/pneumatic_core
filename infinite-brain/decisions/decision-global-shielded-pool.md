---
id: decision-global-shielded-pool
title: "Global shared shielded pool"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Shielded note commitments accumulate in one global pool (not per-token pools); the pool is the canonical note universe for Merkle roots."
auto_inject: false
applicable_when: "Working on the shielded pool, Merkle root state, or S5.3/S5.4 pool phases"
confidence: 1.0
verified_at: "09/22/2026"
verified_by: "dsh-agent"
staleness_signal: "If the pool is split per-token, or the pool's canonicality is challenged in the roadmap"
tags: [shielded, pool, merkle, consensus]
edges:
  - target: source-shielded-roadmap
    type: derived_from
    weight: 1.0
    note: "Key decision in the roadmap"
  - target: concept-shielded-pool
    type: supports
    weight: 1.0
    note: "The pool is the subject of this decision"
  - target: concept-shielded-pool-view
    type: supports
    weight: 0.9
    note: "S5.1 seam; post-S5.3 the real pool's view_parts() feed it — S5.4 swaps in Arc<ShieldedPool> directly"
  - target: decision-nullifier-consensus-critical
    type: related_to
    weight: 0.8
    note: "Pool and nullifier state are the two consensus-critical shielded states"
related: []
source_url: "Empty"
---

# Global shared shielded pool

All shielded note commitments land in **one global pool**, not per-token pools. The pool is the canonical universe of notes, and its incremental Merkle tree is what the shielded verification spec checks (root freshness + recency window). One pool keeps root state singular and consensus-deterministic, and lets a token's shielded activity reference the same root history.

Current state (post-S5.3, 2026-09-22): the real pool exists and is canonical — ONE global `ShieldedPool` (`committer/src/shielded_pool.rs`) is built at boot (`ShieldedPool::load`, fail-closed), appended/persisted/reverted in lockstep with the chain in the commit path, and shared as `Arc<ShieldedPool>` through committer and node-server. Its `view_parts()` (boot-time `Arc<NullifierRegistry>` + `Arc<MerkleRootState>`) currently feed `SimpleShieldedPoolView` for the validation path; **S5.4** is the remaining swap: pass `Arc<ShieldedPool>` (which already `impl ShieldedPoolView`) directly, so the validation path reads the live pool through the same trait seam.
