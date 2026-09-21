---
id: concept-action-circuit
title: "ActionCircuit — the shielded spend circuit"
type: concept
namespace: pneumatic
visibility: namespace
summary: "halo2 circuit proving: spend authorization, Merkle membership of spent notes, well-formed output commitments, and value balance — with fail-closed construction."
auto_inject: false
applicable_when: "Modifying circuit constraints, adding actions, or debugging proof failures"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the circuit's constraint set, action shape, or K changes"
tags: [shielded, halo2, circuit, proving]
edges:
  - target: decision-halo2-no-trusted-setup
    type: derived_from
    weight: 1.0
    note: "Built in halo2 per the no-trusted-setup decision"
  - target: concept-note-commitment
    type: depends_on
    weight: 1.0
    note: "Well-formedness + balance are constraints over commitments"
  - target: concept-incremental-merkle-tree
    type: depends_on
    weight: 0.9
    note: "Verifies Merkle membership paths in-circuit"
  - target: pattern-fail-closed
    type: supports
    weight: 0.9
    note: "Construction is fail-closed: bad inputs fail to build, not to prove"
related: []
source_url: "Empty"
---

# ActionCircuit — the shielded spend circuit

`ActionCircuit` (`src/shielded/circuit.rs:289`, config at `:215`) is the halo2 circuit that makes a shielded spend sound. It asserts, per action:

1. **Spend authorization** — the prover knows the key material for the spent notes (key correctness).
2. **Merkle membership** — each spent note commitment sits in the claimed pool root's tree (membership path verifies in-circuit; an off-circuit cross-check `verify_merkle_path_off_circuit` exists at `:329`).
3. **Well-formed outputs** — output note commitments match their declared components.
4. **Value balance** — Σ value in = Σ value out (via the commitment homomorphism).

Construction is **fail-closed**: structurally invalid inputs (e.g. inconsistent commitments) are rejected at construction time rather than producing a weak proof. Built at `K = 10`.
