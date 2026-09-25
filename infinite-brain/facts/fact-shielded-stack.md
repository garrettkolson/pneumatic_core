---
id: fact-shielded-stack
title: "Shielded ZK stack: module map (S1–S6 landed)"
type: fact
namespace: pneumatic
visibility: namespace
summary: "src/shielded/ = poseidon, note, tree, circuit, verify, roots, pool_view + circuit_test; plus ShieldedTransaction, ShieldedValidationSpec, NullifierRegistry, MerkleRootState, ShieldedPool, prover build/assemble. S1–S6 complete — Tier-1 feature-complete."
auto_inject: false
applicable_when: "Locating shielded code, or continuing the operational open items (circuit audit, viewing keys, proving UX, anonymity bootstrap)"
confidence: 1.0
verified_at: "09/25/2026"
verified_by: "dsh-agent"
staleness_signal: "If a new shielded sub-module appears or a phase beyond S5.2 lands"
tags: [shielded, module-map, zk, architecture]
edges:
  - target: pillar-shielded-value-transfer
    type: supports
    weight: 1.0
    note: "The concrete state of the Tier-1 deliverable"
  - target: fact-workspace-layout
    type: part_of
    weight: 0.8
    note: "Lives in the root crate's shielded/ module"
  - target: source-shielded-roadmap
    type: derived_from
    weight: 0.9
    note: "Phase numbering follows the roadmap"
  - target: event-s5-2-finalizer-wiring
    type: derived_from
    weight: 0.9
    note: "Latest landed phase"
related: []
source_url: "Empty"
---

# Shielded ZK stack: module map

Landed code (verified against the tree):

**`src/shielded/`** — 8 files:
- `poseidon.rs` — Poseidon1 over Pallas Fp (t=5, R_F=8, R_P=46, α=7, Grain-LFSR constants, Cauchy MDS)
- `note.rs` — Pedersen note commitment on Pallas Ep (`hash_to_curve("pneumatic_note_commitment")` → G_v/G_o/G_r/G_rho) + `nullifier(note, spend_key) = poseidon(spend_key, rho)` (line 143)
- `tree.rs` — incremental Merkle tree, `DEFAULT_DEPTH = 32` (line 35)
- `circuit.rs` — `ActionCircuit` (spend auth + Merkle membership + well-formed outputs + value balance; fail-closed construction)
- `verify.rs` — network-side `ShieldedVerifier`; cached `keygen_vk`, `K = 10` (line 399), `Blake2bRead` transcript
- `roots.rs` — `MerkleRootState` (line 70)
- `pool_view.rs` — `ShieldedPoolView` trait (line 44) / `SimpleShieldedPoolView` (line 58); S5.1 read-only seam
- `circuit_test.rs` — `const K: u32 = 10` test mirror

**Elsewhere:** `ShieldedTransaction` (`src/transactions.rs:362`; canonical bytes via rmp, id = SHA-256), `ShieldedValidationSpec` named "Shielded" (`src/validation.rs:446`), `NullifierRegistry` (`src/registry.rs:382`), `ShieldedPool` + apply/revert/persist (`committer/src/shielded_pool.rs`, S5.3), composite real-pool wiring (`node-server/src/node_server.rs`, S5.4), prover `build_shielded_tx`/`assemble_tx` (`prover/src/build.rs`), and the S6 test layer: `tests/shielded_pipeline.rs` (cross-crate 4-hop pipeline + wire-byte privacy), `tests/shielded_attacks.rs` (canonical attack table), concurrency tests in `src/registry/tests/nullifiers.rs` and `src/shielded/tree.rs`, timing test in `prover/src/build.rs`.

Phases S1.1 → S6 are all landed (git: 16ec89c … S6 closeout). Tier-1 is feature-complete; remaining work is operational (see event-s6-shielded-completion).
