---
id: decision-note-based-utxo
title: "Note-based UTXO mirroring Orchard"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Shielded value flows as committed notes (Orchard-style): Pedersen commitment, viewing-key spend, nullifier per note. Mirrors Zcash Orchard's note model."
auto_inject: false
applicable_when: "Designing note fields, spend semantics, or wallet-facing shielded APIs"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the note model changes (e.g. to a balance-based model) or Orchard-style fields are reworked"
tags: [shielded, notes, orchard, utxo]
edges:
  - target: source-shielded-roadmap
    type: derived_from
    weight: 1.0
    note: "Key decision in the roadmap"
  - target: concept-note-commitment
    type: supports
    weight: 1.0
    note: "The note commitment is the concrete instantiation"
  - target: concept-nullifier
    type: supports
    weight: 1.0
    note: "One nullifier per note is the spend-detection mechanism"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.8
    note: "Notes are the value unit inside shielded transactions"
related: []
source_url: "Empty"
---

# Note-based UTXO mirroring Orchard

Shielded value is modeled as **notes** — the Orchard-style committed-UTXO model — rather than account balances. Each note carries value, an owner key, a randomizer (rcm), and a per-note discriminator (rho); it is published as a single Pedersen commitment on Pallas Ep. Spending a note reveals only its **nullifier** (Poseidon over spend key and rho), which is what the network can see and deduplicate.

This was chosen deliberately: it reuses the well-understood Zcash Orchard threat model (linkability limits, nullifier consensus rules) and maps cleanly onto pneumatic's existing per-token chains.
