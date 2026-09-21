---
id: fact-hybrid-wire-format
title: "Hybrid wire format: 3796 B hybrid signature, ~2.3 KB (2332 B) PQC-hybrid KEM ciphertext"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Hybrid sig = [Ed25519 64 B][ML-DSA-44 PK 1312 B][ML-DSA-44 sig 2420 B] = 3796 B; KEM ct = 32+1088+1184+12+16 = 2332 B for empty plaintext."
auto_inject: false
applicable_when: "Estimating message/payload sizes, parsing hybrid signatures or ciphertexts, or checking frame-cap headroom"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the layout constants at src/crypto.rs:53-63 change (MLDSA_PK_LEN / MLDSA_SIG_LEN / MLKEM lengths) or the concatenation order changes"
tags: [wire-format, pqc, mldsa, mlkem, byte-offsets, sizing]
edges:
  - target: concept-hybrid-pq-design
    type: derived_from
    weight: 1.0
    note: "Byte layout is the wire-level projection of the N = N+1 design"
  - target: fact-hybrid-pq-crypto
    type: related_to
    weight: 0.9
    note: "Same algorithms; this node fixes the exact byte offsets"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.8
    note: "2332 B ciphertexts per shielded output stay far under the 16 MiB frame cap"
related: []
source_url: "Empty"
---

# Hybrid wire format: exact byte layouts

The hybrid crypto wire layouts are fixed by the constants documented at src/crypto.rs:53-63:

**Hybrid signature (3796 bytes total):**

| Segment | Length |
|---|---|
| Ed25519 signature | 64 B |
| ML-DSA-44 public key | 1312 B (`MLDSA_PK_LEN`) |
| ML-DSA-44 signature | 2420 B (`MLDSA_SIG_LEN`) |

The public key is carried inline so verification is stateless — the receiver doesn't need to resolve a key ID before checking the PQ half.

**Hybrid KEM ciphertext (2332 bytes for empty plaintext, ≈2.3 KB):**

| Segment | Length |
|---|---|
| X25519 ephemeral public key | 32 B |
| ML-KEM-768 encapsulation | 1088 B |
| ML-KEM-768 recipient public key | 1184 B |
| AES-256-GCM nonce | 12 B |
| Ciphertext + GCM tag | 16 B (empty payload) |

Both halves must verify / decrypt — a failure in either rejects the whole construct. Practical consequence: a shielded transaction with two 512-byte note ciphertexts round-trips through the length-prefixed MsgPack framing with ample headroom under `MAX_FRAME_SIZE` (16 MiB, src/conns.rs:224) — asserted in the wire round-trip test at src/messages.rs:169-241.
