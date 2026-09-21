---
id: fact-hybrid-pq-crypto
title: "Hybrid PQ crypto: (Ed25519·ML-DSA-44) sign, (X25519·ML-KEM-768) KEM"
type: fact
namespace: pneumatic
visibility: namespace
summary: "AsymCryptoProvider is fully hybrid (N = N+1): signatures = Ed25519 + ML-DSA-44 concatenated; key exchange = X25519 + ML-KEM-768. Phase 7."
auto_inject: false
applicable_when: "Touching crypto.rs, adding signing/encryption paths, or discussing post-quantum readiness"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the hybrid scheme changes (different PQC level) or classical primitives are replaced"
tags: [pqc, mldsa, mlkem, hybrid, crypto]
edges:
  - target: event-pq-hybrid
    type: derived_from
    weight: 1.0
    note: "Landed in the Phase-7 hybrid commit"
  - target: fact-rns-pinning
    type: related_to
    weight: 0.7
    note: "pqcrypto crate versions sit in the same dep-pinning regime"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.7
    note: "Signature/KEM substrate for worker messages"
related: []
source_url: "Empty"
---

# Hybrid PQ crypto (N = N+1)

`src/crypto.rs` (Phase 7) makes **every** asymmetric operation a concatenation of a classical and a post-quantum primitive — the "N = N+1" strategy from TLS 1.3 / WireGuard:

- **Signatures:** Ed25519 **·** ML-DSA-44 (detached) — `check_signature`/`sign_data` verify both halves
- **Key exchange:** X25519 **·** ML-KEM-768 — `encrypt`/`decrypt`

`AsymCryptoProvider` is the trait; `Ed25519Provider` is the concrete hybrid implementation (classical Ed25519 via ed25519-dalek + X25519 DH; PQC via `pqcrypto_mldsa::mldsa44` and `pqcrypto_mlkem::mlkem768` — note: 44/768 levels, not the 65/1024). Fixed byte offsets for the hybrid wire formats are defined at the top of `crypto.rs`. Shielded note ciphertexts (~2.3 KB) are encrypted with this hybrid KEM. Audit notes: AUDIT_CHECKLIST.md Phase 7.
