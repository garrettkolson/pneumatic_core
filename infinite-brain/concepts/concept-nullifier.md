---
id: concept-nullifier
title: "Nullifier — one-way spend marker"
type: concept
namespace: pneumatic
visibility: namespace
summary: "nullifier = Poseidon1(spend_key, rho) — a public, consensus-deduplicated, information-free marker that a specific note was spent. Double-spend = double nullifier."
auto_inject: false
applicable_when: "Touching nullifier generation, NullifierRegistry, or double-spend rules"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the nullifier hash inputs change, or nullifiers stop being consensus-checked"
tags: [shielded, nullifier, spend, consensus]
edges:
  - target: decision-nullifier-consensus-critical
    type: derived_from
    weight: 1.0
    note: "The decision elevating nullifiers to consensus state"
  - target: concept-note-commitment
    type: depends_on
    weight: 0.9
    note: "rho (from the note) is one input to the hash"
  - target: concept-transaction-lifecycle
    type: supports
    weight: 0.9
    note: "Membership check is gate 1 of validation"
  - target: fact-shielded-stack
    type: part_of
    weight: 0.7
    note: "Generated in note.rs:143; tracked in registry.rs:382"
related: []
source_url: "Empty"
---

# Nullifier — one-way spend marker

A **nullifier** is the only trace of a spent note:

```
nullifier = Poseidon1(spend_key, rho)
```

(`src/shielded/note.rs:143`). The spend key is derived from the note's key material (owner-held secret); rho is the note's discriminator. Because Poseidon1 is one-way, the nullifier reveals **nothing** about the note's value, owner, or position in the pool — but it is *deterministic*, so spending the same note twice yields the same nullifier, which the network detects and rejects.

Network state: `NullifierRegistry` (`src/registry.rs:382`) provides `NullifierMembership` membership, checked by the validation spec **before** proof verification. Nullifier uniqueness is therefore consensus-critical — the load-bearing wall of double-spend prevention.
