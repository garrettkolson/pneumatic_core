---
id: task-s6-shielded-completion
title: "S6 — Shielded Tier-1 completion: attacks, concurrency, cross-crate pipeline, proving UX"
type: task
namespace: pneumatic
visibility: namespace
summary: "Close the shielded plan: canonical attack suite (S6.2), nullifier/Merkle concurrency (S6.3), cross-crate 4-hop pipeline with wire-byte privacy assertion (S6.1), prove/verify timing benchmark (S6.4). CLOSED 2026-09-25."
auto_inject: false
applicable_when: "Reviewing shielded coverage, auditing the Tier-1 claim, or continuing the operational open items (circuit audit gate, viewing-key policy, proving UX, anonymity bootstrap)"
confidence: 1.0
verified_at: "09/25/2026"
verified_by: "dsh-agent"
staleness_signal: "Closed — S6 entry landed in AUDIT_CHECKLIST.md (2026-09-25)"
tags: [task, shielded, s6, integration, audit, benchmark]
edges:
  - target: source-shielded-plan
    type: derived_from
    weight: 1.0
    note: "Specified in the parent shielded implementation plan; execution plan at plans/S6-implementation-plan.md"
  - target: pillar-shielded-value-transfer
    type: part_of
    weight: 1.0
    note: "Final feature phase of the Tier-1 plan"
  - target: concept-shielded-pool
    type: depends_on
    weight: 0.9
    note: "S6.1's shared pool and S6.2's apply-time re-checks exercise it end-to-end"
  - target: concept-shielded-verifier
    type: depends_on
    weight: 0.8
    note: "S6.4 times its verify path against the 100 ms network budget"
  - target: fact-test-suite-s6
    type: supports
    weight: 0.9
    note: "The 834/37/0 baseline is the post-S6 state"
related: []
source_url: "repo:pneumatic-shielded-implementation-plan.md"
---

# Task: S6 shielded Tier-1 completion (CLOSED 2026-09-25)

Execution plan: `plans/S6-implementation-plan.md`. Ground rules: `cargo check --workspace` + `cargo test --workspace` green after every item, test count monotonic, ≥1 discriminator test per item, fail-closed, live halo2 only in `#[ignore]`d tests, no wire changes, zero new lockfile packages (all S6 deps pre-existed: `tempfile`, `rand`, `ed25519-dalek`, `pasta_curves`, `ff`, `dashmap`, `async-trait`, the four worker-crate dev-deps).

**S6.1 — Cross-crate pipeline (`tests/shielded_pipeline.rs`, new).** Sentinel + finalizer + committer in one process sharing ONE `PendingTransactionRegistry` (the sentinel's `register_shielded` feeds the committer's S5.3 hash check) and ONE `Arc<ShieldedPool>` (advisory readers + single writer). Composite identity pattern: one identity for sentinel+finalizer (finalizer C1 requires the `SignShielded` sender registered as Finalizer), one for the committer. Fast: (1) `wire_bytes_carry_only_the_public_surface` — no note plaintext (owner pk, spend seed, value LE/BE, recipient ed25519/x25519 pks) in ANY recorded wire byte of all three roles, while the public surface (both commitments, nullifier, root, proof) is present in rmp encoding; presence is asserted against DECODED `Message` bodies because the wire is MsgPack (`[u8;32]` → integer arrays, `Message.body` → per-byte integers, so the stx rmp only travels contiguously inside decoded bodies — the load-bearing wire-encoding fact for the privacy claim); (2) `sentinel_advisory_gate_rejects_double_spend_before_finalizer_work` — real spec, pre-spent nullifier, `StaleNullifier` before registration/wire traffic (checks 1→3 short-circuit before check 4 ⇒ no keygen). Live `#[ignore]`d: fake-proof tx dies exactly at the committer's pool re-check (`InvalidShieldedProof`, pool untouched); real prove (100 = 90 + 10) commits with pool delta and chain in lockstep (leaf +1, delta +1, nullifier spent, root tip == rebuilt tree, chain +1, committed block == tip), output note pays 90, replay is idempotent `AlreadyApplied`.

**S6.2 — Canonical attack suite (`tests/shielded_attacks.rs`, new).** Audit table mapping the six canonical shielded-transfer attacks to canonical coverage (S4.1.5 spec checks; S5.3 committer re-check; pool apply) + new tests: fast `pool_rejects_stale_nullifier_at_apply` / `pool_rejects_stale_root_at_apply` (apply re-check fires checks 2/3 before check 4 — no keygen); `#[ignore]`d `truncated_proof_rejected_not_panicked` (malformed input → typed rejection, never panic — the verifier is total) and `concurrent_same_note_spend_exactly_one_commits` (race → exactly one `Applied`, loser `StaleNullifier`, loser's leaf absent).

**S6.3 — Concurrency at the shared-state seams.** `src/registry/tests/nullifiers.rs`: `concurrent_try_mark_spent_50_threads_mixed_unique_and_duplicates` (50 threads, 25 unique + duplicates → exactly 25 `Ok`). `src/shielded/tree.rs`: `concurrent_append_and_proof_verification_stay_consistent` (4 readers verify membership proofs against the current root while 200 appends race — proof/root pair never diverges under the tree lock).

**S6.4 — Proving UX (`prover/src/build.rs` test module).** `build_shielded_tx_prove_and_verify_timing` (`#[ignore]`, `std::time::Instant`, no criterion): warm prove + verifier build untimed, then a TIMED prove of a fresh 1-in/1-out at k=10 and a TIMED network-side verify; prints the audit line and asserts a 200 ms verify tripwire (the 100 ms Tier-1 plan target measured 124 ms — unmet within box noise — so the assert is ~1.6× measured and trips on a 2×+ regression). Measured: release **prove 25.4 s / verify 124 ms**; debug prove 247 s / verify 1.14 s (~100× inflation, hence `--release`).

**Outcome:** workspace 834 passed / 37 ignored / 0 failed (from the 828/32/0 baseline: +6 core passed, +4 core ignored, +1 prover ignored). Wire: zero changes. Lockfile: zero new packages. The four new live tests join the benchmark lane. Tier-1 is now FEATURE-COMPLETE; what remains is operational, not code — see the "Open items at close" in the AUDIT_CHECKLIST S6 entry: (1) independent circuit audit as a launch gate, (2) viewing-key policy, (3) proving-UX product decision (embedded vs prover service) given the measured prove time, (4) anonymity-set genesis policy.
