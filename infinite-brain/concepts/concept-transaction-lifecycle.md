---
id: concept-transaction-lifecycle
title: "Shielded transaction lifecycle & validation gates"
type: concept
namespace: pneumatic
visibility: namespace
summary: "ShieldedTransaction (transactions.rs:362) validation = 4 fail-closed gates: structural, nullifier membership, merkle-root freshness, proof verify. Id = SHA-256 of rmp canonical bytes."
auto_inject: false
applicable_when: "Adding validation checks, modifying the shielded tx struct, or debugging rejections"
confidence: 1.0
verified_at: "09/23/2026"
verified_by: "dsh-agent"
staleness_signal: "If the 4-check structure of ShieldedValidationSpec changes"
tags: [shielded, validation, lifecycle, transactions]
edges:
  - target: concept-nullifier
    type: depends_on
    weight: 0.9
    note: "Gate 2: nullifier membership"
  - target: concept-merkle-root-freshness
    type: depends_on
    weight: 0.9
    note: "Gate 3: root freshness + recency"
  - target: concept-shielded-verifier
    type: depends_on
    weight: 0.9
    note: "Gate 4: proof verification"
  - target: pattern-fail-closed
    type: supports
    weight: 1.0
    note: "All 4 gates fail closed"
related: []
source_url: "Empty"
---

# Shielded transaction lifecycle & validation gates

`ShieldedTransaction` (`src/transactions.rs:362`) carries: `id`, `action`, `token_id`, `spent_commitments`, `nullifiers`, `commitments` (outputs), `merkle_root`, `proof`, `note_ciphertexts`, `fee`. Identity is canonical: `canonical_bytes()` (line 409) serializes via **rmp** (MsgPack), and the tx id is **SHA-256** of those bytes (line 419).

Network-side validation is `ShieldedValidationSpec` named `"Shielded"` (`validation/shielded.rs:63`, registered at `validation/registries.rs:49`), running **four fail-closed gates in order**:

1. **Structural** — well-formed fields
2. **Nullifier membership** — via `NullifierMembership` (no double-spend)
3. **Merkle-root freshness** — via `MerkleRootState` history + recency window
4. **Proof verification** — `ShieldedVerifier`

Risk accounting treats it as a neutral-risk action (2 parties, 0 public amount). Finalizer wiring (S5.2) makes the spec part of the block path.
