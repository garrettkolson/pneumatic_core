---
id: decision-nullifier-consensus-critical
title: "Nullifiers are consensus-critical"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Nullifier uniqueness is enforced at consensus (NullifierMembership) — double-spend = double nullifier = invalid block. The core privacy/consensus tradeoff."
auto_inject: false
applicable_when: "Touching validation, nullifier handling, or designing new spend types"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If nullifier checks move out of the validation spec, or a non-consensus nullifier mechanism is introduced"
tags: [shielded, nullifier, consensus, validation]
edges:
  - target: source-shielded-roadmap
    type: derived_from
    weight: 1.0
    note: "Key decision in the roadmap"
  - target: concept-nullifier
    type: supports
    weight: 1.0
    note: "Defines what a nullifier is"
  - target: concept-transaction-lifecycle
    type: supports
    weight: 0.9
    note: "Nullifier check is one of the 4 fail-closed validation gates"
  - target: pattern-fail-closed
    type: supports
    weight: 0.8
    note: "The membership check is fail-closed by construction"
related: []
source_url: "Empty"
---

# Nullifiers are consensus-critical

Every spent note produces exactly one nullifier, and the network **must** enforce nullifier uniqueness at the consensus layer. A double-spend is indistinguishable from a double-nullifier, so the check lives inside the shielded validation spec: `NullifierMembership` membership (`src/registry.rs:382`) is checked before the zk-proof is even verified.

This is the fundamental privacy/consensus tradeoff of the design: nullifiers are public, on-chain, consensus-checked data. Privacy is achieved by making the nullifier a one-way Poseidon hash (spend key, rho) that reveals nothing about the note — but the *uniqueness obligation* is fully public and consensus-critical.
