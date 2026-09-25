---
id: pillar-shielded-value-transfer
title: "Shielded value transfer (Tier-1 deliverable)"
type: pillar
namespace: pneumatic
visibility: namespace
summary: "Tier-1 roadmap goal: private token value transfer via note commitments, nullifiers, and halo2 zk-proofs verified on-chain; S1–S6 all landed 09/25 — feature-complete, only operational items open (circuit audit gate, viewing-key policy, proving UX, anonymity bootstrap)."
auto_inject: false
applicable_when: "Any work on the shielded/ module, shielded transactions, or the S-phase roadmap"
confidence: 0.95
verified_at: "09/25/2026"
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

State as of 09/25/2026: **all S-phases S1–S6 are landed — Tier-1 is feature-complete.** The landed stack: halo2 circuit, Poseidon1 hashing, note commitment, spend/nullifier logic, Merkle membership, network-side verifier (cached vk), shielded transaction type, fail-closed 4-gate validation spec, real pool append with durable nullifiers (S5.3), the composite node's real shared `Arc<ShieldedPool>` (S5.4), and the S6 completion layer — cross-crate 4-hop pipeline with wire-byte-level privacy assertion, canonical attack suite, nullifier/Merkle concurrency proofs, and prove/verify timing (834/37/0 baseline). What remains is OPERATIONAL, not code: (1) independent ActionCircuit audit as a launch gate, (2) viewing-key policy, (3) proving-UX product decision (embedded wallet vs prover service) given the measured prove time, (4) anonymity-set genesis policy — see the AUDIT_CHECKLIST S6 "Open items at close".
