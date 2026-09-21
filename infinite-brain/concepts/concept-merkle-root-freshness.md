---
id: concept-merkle-root-freshness
title: "Merkle-root freshness + recency window"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Validation requires the tx's claimed Merkle root to exist in MerkleRootState history and to be within a recency window — old roots (and thus old membership proofs) are rejected."
auto_inject: false
applicable_when: "Tuning the recency window, adding root history checks, or debugging 'stale root' rejections"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the recency window size, history source, or the freshness check is removed"
tags: [shielded, merkle, freshness, validation]
edges:
  - target: concept-incremental-merkle-tree
    type: depends_on
    weight: 1.0
    note: "Checks the tree's root history"
  - target: concept-shielded-pool
    type: depends_on
    weight: 0.9
    note: "Roots are the pool's public state"
  - target: concept-transaction-lifecycle
    type: supports
    weight: 0.9
    note: "One of the 4 fail-closed gates"
related: []
source_url: "Empty"
---

# Merkle-root freshness + recency window

A shielded transaction claims a Merkle root (the pool root under which its spent-note membership proofs are valid). The validation spec checks two things about that root:

1. **Existence** — the claimed root appears in `MerkleRootState` history (`src/shielded/roots.rs:70`); an unknown root is rejected.
2. **Recency** — the root must be **within the recency window** (recent enough). A root from the deep past is rejected even if it once existed.

Why it matters: without recency, a stale membership proof (captured when the pool was small) would remain spendable forever; the window forces proofs to track the current pool state. Together with nullifier membership, this is what makes the *off-proof* part of validation cheap and deterministic.
