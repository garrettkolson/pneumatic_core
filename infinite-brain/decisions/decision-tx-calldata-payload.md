---
id: decision-tx-calldata-payload
title: "ADR-012: Additive payload: Vec<u8> on Transaction as contract calldata"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Additive payload: Vec<u8> on Transaction carries contract calldata (skip-if-empty keeps legacy wire byte-identical); it joins CanonicalTransaction so the sender signs it, with a sentinel-enforced size cap."
auto_inject: false
applicable_when: "Adding contract input data to transactions, changing the canonical transaction form, or touching wire compatibility"
confidence: 0.95
verified_at: "09/27/2026"
verified_by: "Garrett Olson"
staleness_signal: "Stale when payload is removed from Transaction, moves out of CanonicalTransaction, or a sidecar calldata mechanism replaces it"
tags: [adr, design-decision, calldata, wire-protocol, transaction]
edges:
  - target: concept-transaction-lifecycle
    type: related_to
    weight: 0.85
    note: "Calldata is part of the transaction's signed canonical identity"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.8
    note: "Additive rmp field under the 4-byte-length MsgPack frame protocol"
  - target: concept-transaction-state-machine
    type: related_to
    weight: 0.7
    note: "Payload rides the same TransactionState pipeline as all tx fields"
  - target: task-executor-contract-execution
    type: supports
    weight: 0.8
    note: "Phase 1 of the implementation plan lands this field"
related: []
source_url: "plans/executor-contract-execution-implementation-plan.md (Q2)"
---

# ADR-012: Additive `payload: Vec<u8>` on Transaction as contract calldata

Approved by Garrett Olson, 09/27/2026.

Contract input travels in a new field `pub payload: Vec<u8>` on `Transaction`
(`src/transactions.rs:162-192`), declared
`#[serde(default, skip_serializing_if = "Vec::is_empty")]` — the Ground Rule 4
pattern (precedent: `SignedTransaction.shielded`): an empty payload serializes to
nothing, so every existing transaction's rmp bytes stay identical on the wire, and
legacy messages deserialize to `vec![]`.

Three binding refinements:

1. **`payload` joins `CanonicalTransaction`** — the sender's signature
   (`verify_sender_signature`) must cover calldata; without it a relay can swap
   calldata on a signed tx (malleability).
2. **Sentinel-enforced size cap** (env-configurable, a few KB) bounds the DoS
   surface and the canonical-hash input; payload bytes feed the gas computation
   (ADR-013).
3. **Client convention**: `tx.id` is derived from the canonical form *including*
   payload, so calldata-differing txs never collide in the pending registry.

Note: `Bid { bid_expiry, bid_percentage }` (`src/transactions.rs:196-200`) is a
price structure (percentage-of-amount rate with expiry), not a gas structure — the
two axes stay orthogonal.

**Alternatives rejected**: encoding calldata in `action` (overloads a routing key,
33% hex bloat, unbounded); sidecar calldata outside the tx (not covered by the sender
signature); deferring the field until `SpecEngine` needs it (same exercise twice).
