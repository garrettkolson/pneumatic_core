---
id: concept-note-commitment
title: "Note commitment (Pedersen on Pallas Ep)"
type: concept
namespace: pneumatic
visibility: namespace
summary: "C = G_v·value + G_o·owner + G_r·rcm + G_rho·rho — a Pedersen commitment over Pallas Ep with four hash-to-curve generators, binding the note's four secret terms."
auto_inject: false
applicable_when: "Touching note.rs, commitment arithmetic, or shielded outputs"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the commitment equation, field, curve, or domain prefix changes"
tags: [shielded, pedersen, commitment, pallas]
edges:
  - target: decision-note-based-utxo
    type: derived_from
    weight: 1.0
    note: "The concrete commitment for the note-based model"
  - target: concept-incremental-merkle-tree
    type: supports
    weight: 0.9
    note: "Commitments are the leaves of the pool's Merkle tree"
  - target: concept-action-circuit
    type: supports
    weight: 0.9
    note: "The circuit enforces well-formed commitments and value balance"
  - target: fact-shielded-stack
    type: part_of
    weight: 0.8
    note: "Implemented in src/shielded/note.rs"
related: []
source_url: "Empty"
---

# Note commitment (Pedersen on Pallas Ep)

A shielded note is published as a single Pedersen commitment over **Pallas Ep**:

```
C = G_v·value + G_o·owner_pk_scalar + G_r·rcm + G_rho·rho
```

The four generators are **distinct** Ep points, each derived via `Ep::hash_to_curve("pneumatic_note_commitment")` with a shared domain prefix and per-term sub-labels (`src/shielded/note.rs:60-78`). The four secret terms: note **value**, owner key scalar, blinding **rcm**, and per-note **rho** (the nullifier discriminator).

Because the commitment is homomorphic in each term, spend actions can prove `Σ C_in = G_v·Σvalue_in + … = Σ C_out` in-circuit without revealing value, owner, rcm, or rho — the algebraic core of the privacy guarantee.
