---
id: concept-shielded-verifier
title: "ShieldedVerifier (network side, cached vk)"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Network-side halo2 verifier: keygen_vk cached once per process, SingleVerifier with Blake2bRead transcript, K=10; verifies ActionCircuit proofs in milliseconds."
auto_inject: false
applicable_when: "Modifying verify.rs, adding proof verification paths, or tuning verifier performance"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the verifier's transcript, K, or key-caching strategy changes"
tags: [shielded, verifier, halo2, performance]
edges:
  - target: decision-client-side-proving
    type: derived_from
    weight: 1.0
    note: "The verify-only half of the client-side-proving decision"
  - target: concept-action-circuit
    type: depends_on
    weight: 1.0
    note: "Verifies the ActionCircuit's proofs"
  - target: concept-transaction-lifecycle
    type: supports
    weight: 0.9
    note: "Proof verification is one of the 4 validation gates"
related: []
source_url: "Empty"
---

# ShieldedVerifier (network side)

`src/shielded/verify.rs` is the **network-side** verifier. Design points:

- **Cached keygen_vk** — `keygen_vk` (the expensive part, ~seconds) runs exactly once per process; the `VerifyingKey` is cached so repeated verifications skip it (`verify.rs:70-76`).
- **`SingleVerifier` + `Blake2bRead` transcript** — the lightweight halo2 verification path with the standard 255-bit challenge read.
- **`K = 10`** — the verifier's `Params` mirrors `const K: u32 = 10` in `circuit_test.rs` (`verify.rs:399`).

This is what makes the "clients prove, network verifies" split economically viable: verification is millisecond-scale and runs inside the shielded validation spec.
