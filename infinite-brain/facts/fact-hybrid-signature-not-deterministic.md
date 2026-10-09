---
id: fact-hybrid-signature-not-deterministic
title: "sign_data is NOT deterministic — the ML-DSA half signs with fresh randomness, so the same key over the same bytes yields a different signature every time (the code comment said otherwise)"
type: fact
namespace: architecture
visibility: namespace
summary: "10/09/2026, measured while writing the Committer's routing test. `Ed25519Provider::sign_data` returns a 3,796-byte hybrid signature = Ed25519(64) ‖ ML-DSA public key ‖ ML-DSA signature. The Ed25519 half is deterministic; the ML-DSA half is not — PQClean's entry point signs with fresh randomness — so `sign_data(m)` twice from one key returns two DIFFERENT byte strings for identical `m` (asserted equal by a test that assumed otherwise; the assertion failed). The comment at `src/crypto.rs` claimed \"ML-DSA half (deterministic)\" and has been corrected in place. Nothing may compare signatures byte-for-byte, deduplicate by signature bytes, or treat a signature as an identifier: `check_signature` is the only sound question to ask of one. Where two values must be tested for stating the same thing, compare the signed fields — `SelectionRecord::same_decision_as` exists because of this (it compares finalizer, attester, epoch and salt and deliberately ignores `attestation`)."
auto_inject: false
applicable_when: "Comparing two signatures or two signed records; deduplicating by signature; caching by signature bytes; asserting a signature equals a previously produced one; any code reading `SelectionRecord.attestation` or `TransactionSignature`; changing the hybrid signature format"
confidence: 0.95
verified_at: "10/09/2026"
verified_by: "dsh-agent"
staleness_signal: "If `sign_data` gains a deterministic-variant entry point (ML-DSA seeded signing with a fixed seed derived from the message), byte-stable signatures become available and the comparison rules relax — re-measure before relying on that, because the wire format is shared with peers that randomize. Also re-check if the ML-DSA backend changes (PQClean → rust-kem / dalek-style bindings)."
tags: [fact, crypto, hybrid-pq, ml-dsa, signatures, determinism, committer, routing]
edges:
  - target: fact-hybrid-pq-crypto
    type: related_to
    weight: 0.95
    note: "The hybrid signature format this measures — Ed25519 ‖ ML-DSA pk ‖ ML-DSA sig, 3,796 bytes"
  - target: decision-deterministic-per-tx-routing
    type: related_to
    weight: 0.9
    note: "The Committer's routing gate had to compare decisions, not attestations, because of this"
  - target: fact-hybrid-wire-format
    type: related_to
    weight: 0.7
    note: "Same primitive on the wire; the size is a wire-format fact too"
related: []
source_url: "repo:src/crypto.rs (sign_data)"
---

# Hybrid signatures are randomized, and a comment said they were not

## Measured, not inferred

```text
sign_data(b"same message") → 3,796 bytes
sign_data(b"same message") → 3,796 bytes
equal?                   → false
```

Same key, same message, two calls, two different byte strings. The first version of
`committer::tests::routing::a_block_carrying_a_different_decision_for_the_same_transaction_is_refused`
compared two `SelectionRecord`s with `==` and failed while printing **identical**
finalizer and attester keys — the only fields left were the attestation bytes, which
led here.

## Why

`src/crypto.rs::sign_data` concatenates three parts:

| Part | Length | Deterministic? |
|---|---|---|
| Ed25519 signature | 64 | yes (RFC 8032, key + message) |
| ML-DSA-44 public key | 1,312 | yes (it is a key) |
| ML-DSA-44 signature | 2,420 | **no** — PQClean's `…_signature()` signs with fresh randomness |

The comment above the third part read `// ML-DSA half (deterministic)`. It has been
replaced with a statement of what was measured, because a wrong comment about a
cryptographic primitive is worse than no comment: it invites exactly the byte
comparison that this fact forbids.

## What follows

- **Do not** compare signatures with `==`, use them as map keys, or deduplicate
  messages by signature bytes. Two honest re-signings of one message differ.
- **Do** ask `check_signature(sig, key, msg)`. That is the property the signature
  carries.
- To ask "do these two signed values state the same thing", compare the fields the
  signature covers. `SelectionRecord::same_decision_as` does this for the routing
  record (finalizer, attester, epoch, salt) and documents that the attestation is
  excluded on purpose.
- A test that signs twice and expects equality is a bug in the test.

## Not fixed, and why not

Making the ML-DSA half deterministic (seeded randomness derived from the message, as
a deterministic-variant API would allow) is a wire-protocol change that has to be
made in lockstep with every peer, and nothing in the protocol needs determinism. The
behavior stays; the comment and the comparison sites are what changed.
