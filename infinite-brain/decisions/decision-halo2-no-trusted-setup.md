---
id: decision-halo2-no-trusted-setup
title: "Halo2, no trusted setup"
type: decision
namespace: pneumatic
visibility: namespace
summary: "zk-SNARK stack is halo2 (plonk) with no trusted setup; verifier uses cached keygen_vk and a Blake2bRead transcript with SingleVerifier."
auto_inject: false
applicable_when: "Choosing circuit libraries, changing K, or discussing key generation"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the project migrates to a trusted-setup system (e.g. Groth16) or changes K"
tags: [halo2, snark, no-trusted-setup, shielded]
edges:
  - target: source-shielded-roadmap
    type: derived_from
    weight: 1.0
    note: "Recorded as a key decision"
  - target: concept-action-circuit
    type: supports
    weight: 1.0
    note: "ActionCircuit is implemented in halo2"
  - target: concept-shielded-verifier
    type: supports
    weight: 0.9
    note: "Verifier is built around keygen_vk caching"
  - target: decision-note-based-utxo
    type: related_to
    weight: 0.6
    note: "Same decision set for the shielded stack"
related: []
source_url: "Empty"
---

# Halo2, no trusted setup

The shielded stack uses **halo2 (plonk)** specifically because it needs **no trusted setup** — no ceremony, no ceremony participants, and the verifying key is deterministically derived from the circuit (`keygen_vk`). This was chosen over Groth16-class systems where a trusted setup would be a standing security liability for a consensus-critical proof.

The network-side verifier (`src/shielded/verify.rs`) exploits this: `keygen_vk` runs exactly once per process and the `VerifyingKey` is cached; repeated verifications use `SingleVerifier` with a `Blake2bRead` transcript. The circuit is built at `K = 10` (mirrored by `const K: u32 = 10` in `circuit_test.rs`).
