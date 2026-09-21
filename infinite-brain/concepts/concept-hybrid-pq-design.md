---
id: concept-hybrid-pq-design
title: "Hybrid PQ crypto design: N = N+1 (classical + post-quantum halves)"
type: concept
namespace: pneumatic
visibility: namespace
summary: "pneumatic signs with Ed25519 + ML-DSA-44 and key-exchanges with X25519 + ML-KEM-768 + AES-256-GCM; 'N = N+1' means both halves must verify."
auto_inject: false
applicable_when: "Touching src/crypto.rs, message signing, envelope verification, or any encryption/encryption_to path"
confidence: 0.95
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If src/crypto.rs changes the signature/KEM algorithm pairing or the verify logic stops requiring both halves"
tags: [crypto, pqc, ed25519, mldsa, mlkem, x25519, aes-gcm]
edges:
  - target: fact-hybrid-pq-crypto
    type: derived_from
    weight: 1.0
    note: "The concrete algorithm/version facts behind this design"
  - target: fact-hybrid-wire-format
    type: derived_from
    weight: 0.9
    note: "Wire byte layouts are the observable consequence of the N+1 design"
  - target: pattern-pinned-dependencies
    type: related_to
    weight: 0.7
    note: "pqcrypto-mldsa/mlkem exact pins keep hybrid behavior reproducible"
related: []
source_url: "Empty"
---

# Hybrid PQ crypto design: N = N+1

pneumatic's `AsymCryptoProvider` (src/crypto.rs) composes classical and post-quantum primitives so a node's signature/keystream is only as strong as the half an attacker *doesn't* break:

- **Signing**: Ed25519 (classical) + ML-DSA-44 (NIST FIPS 204). A signature is only valid if **both** halves verify; verification rejects if either half fails.
- **Key exchange / encryption**: X25519 ECDH + ML-KEM-768 (FIPS 203), combined via AES-256-GCM.

The doc comment in `src/crypto.rs:32-48` states the "N = N+1" rationale directly: the two halves are independent, so a classical break (quantum computer) invalidates one half while the PQ half still protects, and a PQ break (algorithm flaw) is covered by the classical half. Neither half alone is sufficient — dropping either defeats the post-quantum property, and keeping both keeps cost bounded (hybrid signatures total 3796 B; see the wire-format fact node).

The layout is fixed and documented at `src/crypto.rs:53-63` (e.g., `MLDSA_PK_LEN = 1312`, `MLDSA_SIG_LEN = 2420` for level-44), which the wire format depends on directly. `NodeIdentity` (src/rns/identity.rs) binds the transport and on-chain keypairs to this provider, and `Message::signed` (src/messages.rs:47-62) is the production envelope built on it.
