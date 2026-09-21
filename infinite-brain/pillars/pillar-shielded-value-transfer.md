---
id: pillar-shielded-value-transfer
title: "Shielded value transfer (Tier-1 deliverable)"
type: pillar
namespace: pneumatic
visibility: namespace
summary: "Tier-1 roadmap goal: private token value transfer via note commitments, nullifiers, and halo2 zk-proofs verified on-chain; phases S1.1–S5.2 landed, S5.3+ pending."
auto_inject: false
applicable_when: "Any work on the shielded/ module, shielded transactions, or the S-phase roadmap"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If a new S-phase beyond S5.2 lands, or the Tier-1 scope changes in the roadmap"
tags: [shielded, roadmap, deliverable, privacy]
edges:
  - target: source-shielded-roadmap
    type: derived_from
    weight: 1.0
    note: "Tier-1 definition and S-phase plan"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.9
    note: "Rides on the block-lattice as a new transaction type"
  - target: fact-shielded-stack
    type: supports
    weight: 1.0
    note: "Current state of the landed shielded stack"
  - target: question-viewing-keys-compliance
    type: depends_on
    weight: 0.7
    note: "Open product decision that shapes the remaining work"
related: []
source_url: "Empty"
---

# Shielded value transfer (Tier-1 deliverable)

The project's Tier-1 deliverable is **shielded value transfer**: users spend and receive token value through private note commitments instead of public UTXOs, with validity proven by zk-SNARKs that the network can verify cheaply. Tier-2 (zk-VM general computation) is explicitly out of scope.

State as of 09/20/2026: phases S1.1–S5.2 are landed — halo2 circuit, Poseidon1 hashing, note commitment, spend/nullifier logic, Merkle membership, network-side verifier, shielded transaction type, fail-closed validation spec, pool view seam, and finalizer wiring. Remaining: S5.3 (real pool append path), S5.4 (swap `SimpleShieldedPoolView` for the real `Arc<ShieldedPool>`), and the S6 phases.
