---
id: event-pq-hybrid
title: "Phase 7: hybrid PQ crypto landed"
type: event
namespace: pneumatic
visibility: namespace
summary: "Commit 503c8b6: the N = N+1 hybrid crypto — every signature (Ed25519·ML-DSA-44) and KEM (X25519·ML-KEM-768) is a classical+PQC concatenation."
auto_inject: false
applicable_when: "Checking PQ readiness status, crypto upgrade history, or audit references"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Historical event — never goes stale"
tags: [event, pqc, crypto, milestone]
edges:
  - target: fact-hybrid-pq-crypto
    type: supports
    weight: 1.0
    note: "The commit that established the hybrid scheme"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.6
    note: "Crypto substrate of the protocol"
related: []
source_url: "git:503c8b6"
---

# Phase 7: hybrid PQ crypto

Commit **503c8b6** landed the **Phase-7** post-quantum work: `src/crypto.rs` became fully **hybrid** under the "N = N+1" strategy — every signature is Ed25519 **and** ML-DSA-44, every key exchange is X25519 **and** ML-KEM-768, concatenated in fixed wire formats.

Wire-shape and interop notes for this phase are tracked in AUDIT_CHECKLIST.md (Phase 7). Shielded note ciphertexts (~2.3 KB each) are encrypted with this hybrid KEM, which is why they clear the 16 MB frame cap with large margin.
