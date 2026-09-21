---
id: concept-incremental-merkle-tree
title: "Incremental Merkle tree (depth 32)"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Append-only Merkle tree (DEFAULT_DEPTH=32) over shielded note commitments; produces the pool roots and membership paths the ActionCircuit checks."
auto_inject: false
applicable_when: "Working on tree.rs, MerkleRootState, membership proofs, or pool roots"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If DEFAULT_DEPTH changes, the leaf hashing changes, or the tree is replaced"
tags: [shielded, merkle, tree, membership]
edges:
  - target: concept-shielded-pool
    type: part_of
    weight: 1.0
    note: "The pool's structure is this tree"
  - target: concept-note-commitment
    type: depends_on
    weight: 0.9
    note: "Leaves are note commitments"
  - target: concept-action-circuit
    type: supports
    weight: 1.0
    note: "The circuit verifies membership paths in-circuit"
  - target: concept-merkle-root-freshness
    type: supports
    weight: 0.9
    note: "Roots feed the freshness/recency check"
related: []
source_url: "Empty"
---

# Incremental Merkle tree (depth 32)

`src/shielded/tree.rs` implements an **incremental Merkle tree**: append-only, fixed depth `DEFAULT_DEPTH = 32` (line 35; `Tree::default()` uses it). Each new note commitment is inserted at the next leaf slot; the root updates in O(depth) without re-hashing prior leaves.

Two consumers: (1) the **network** — `MerkleRootState` (`src/shielded/roots.rs:70`) records root history so the validation spec can check that a transaction's claimed root is fresh and within the recency window; (2) the **prover** — membership paths from this tree are inputs to the `ActionCircuit`, which verifies them in-circuit.
