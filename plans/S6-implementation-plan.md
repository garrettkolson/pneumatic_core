# Implementation Plan — Phase S6 (remaining items of `pneumatic-shielded-implementation-plan.md`)

Parent doc: `pneumatic-shielded-implementation-plan.md` (repo root). This is the
execution plan for what it still contains.

## Status (code-verified — supersedes the doc's (DONE) markers)

| Item | Status | Evidence |
|---|---|---|
| S1–S4 | landed | per parent doc; core `src/shielded/*`, validation spec, `NullifierRegistry` |
| S5.1 | landed | `sentinel/src/sentinel/shielded.rs:42` `handle_shielded_transfer` |
| S5.2 | landed | `finalizer/src/finalizer/shielded.rs:99,166` `handle_sign_shielded`/`handle_shielded_vote` |
| S5.3 | landed | `committer/src/shielded_pool.rs` (`apply_update`, `revert_update`, fail-closed `load`); commit/finalize hooks at `committer/src/committer/committing.rs:170,250` and `finalizing.rs:114` |
| S5.4 | landed | `node-server/src/node_server/build.rs:122` + `plugins.rs:29-37` (one `Arc<ShieldedPool>` shared by all roles) |
| **S6.1–S6.4** | **OPEN — scope of this plan** | no `tests/shielded_pipeline.rs` / `shielded_attacks.rs` / benches; no wire-privacy scan; nullifier concurrency tests exist only at 8/16 threads (`src/registry/tests/nullifiers.rs:125-193`) |

Existing live (already `#[ignore]`d) e2e to build on, not duplicate:
`sentinel` `shielded_transfer_live_proof_end_to_end` (sentinel/src/sentinel/tests/shielded.rs:613)
and `node-server` `live_composite_shielded_e2e_all_four_hops_advance_the_pool`
(node-server/src/node_server/tests/e2e.rs:22).

## Ground rules (inherited from the parent doc — no exceptions)

1. After **every item**: `cargo check --workspace` + `cargo test --workspace` green; test count monotonically increases from the measured baseline.
2. Every item ships ≥1 discriminator test (fails without the change).
3. Fail closed; live halo2 proving only in `#[ignore]`d tests (roadmap Part 5).
4. No wire changes in S6 — ground rule 4 trivially satisfied (recorded, no action).

## Step 0 — Measure the live baseline

Run `cargo test --workspace`; record exact per-crate counts. The vault's 09/23
baseline is **828 passed / 32 ignored** — S6 progression is measured from the
live run, not that number.

## S6.1 — End-to-end integration test

- **Files**: `tests/shielded_pipeline.rs` (root crate, sibling of `transport_integration.rs`;
  worker crates added as `pneumatic_core` dev-dependencies). Follow `committer/tests/pipeline_integration.rs`
  fixture conventions (recorded peers, seeded pool state).
- **Action**: full pipeline in-process — prover `build_shielded_tx` → sentinel
  verify+route → finalizer quorum over public outputs → committer appends block,
  applies pool delta (tree + nullifiers). Two variants, mirroring the S3.3
  split:
  1. **Default-suite test** — same pipeline with a canonical (non-live) proof
     buffer; asserts routing, pool delta, and the **wire-privacy scan**:
     serialize every inter-role message and assert the observer-visible surface
     is ONLY (nullifiers, commitments, roots, proof bytes, tx hash, votes) —
     no sender/receiver/amount bytes anywhere.
  2. **`#[ignore]`d live variant** — one real `build_shielded_tx` proof
     (~1 min) through the same pipeline.
- **Verify**: pipeline assertions pass; **headline discriminator**: a tampered
  fixture with a plaintext amount/memo byte in a wire body → the scan fails
  (a future leak regression fails this test).
- **Done**: both tests land; count progression recorded.

## S6.2 — Double-spend & adversarial suite

- **Files**: `tests/shielded_attacks.rs` (root crate; per-case gate where the
  gate's owner is the right home — committer-level cases may instead live in
  `committer/src/committer/tests/pool.rs` siblings; choose per case, one
  canonical location, no duplication).
- **Action**: first audit existing coverage (committer pool tests already
  assert cross-block double-spend, stale root, idempotent replay); add ONLY
  the missing cases, so the six stand in one suite:
  (a) replay of a spent nullifier → `StaleNullifier`;
  (b) two concurrent txs, same note → exactly one commits
      (the `mark_many_atomic` TOCTOU discriminator);
  (c) proof against a root older than K → `StaleMerkleRoot`;
  (d) malformed/truncated proof → `Err`, never a panic;
  (e) value-imbalance witness → rejected (use S2 negative vector if one
      exists; if S2 never produced one, record the gap in the checklist entry
      and cover the imbalance at the off-circuit balance check instead);
  (f) shielded tx against a non-shielded token → rejected.
- **Verify**: all six rejected with the exact named `ValidationFailureReason`
  / error; (b) is its own discriminator.
- **Done**: suite green; count progression recorded.

## S6.3 — Concurrency stress

- **Files**: `src/registry/tests/nullifiers.rs` (50-thread `try_mark_spent` on
  mixed unique/dup nullifiers → exactly the unique set marked, 0 panics — the
  existing 8/16-thread tests stay as-is); `src/shielded/tree.rs` test module
  (concurrent `verify_proof` readers + `append` writers → no data race; run
  once under `--release`; Miri = documented follow-up, not a gate).
- **Verify**: stress assertions hold (same standard as
  `concurrent_acquire_release_stress_50`).
- **Done**: both stress tests land; count progression recorded.

## S6.4 — Proving/verification benchmarks

- **Files**: `prover/benches/shielded_bench.rs` (bench-gated, excluded from
  the default suite like the live-prove tests).
- **Action**: at the final circuit (K=10, 1-in/1-out): measure (1) client-side
  `build_shielded_tx` proving time, (2) network-side `ShieldedVerifier::verify`
  time. **Assert verify < 100 ms**; record both numbers in the checklist
  entry; report to the roadmap owner (open question #3 — proving UX on wallet
  hardware). If verify blows 100 ms, that is a circuit-design finding to
  escalate, not a silent accept.
- **Verify**: numbers recorded; assertion gate passes.
- **Done**: bench entry recorded.

## Sequencing & parallelism

S6.2 ∥ S6.3 (independent) → S6.1 (reuses adversarial/pool fixtures) → S6.4
last, benchmarking the final state. S6.4 is the only item that can surface a
non-test-only finding.

## Closeout (whole-plan verification, per parent doc "Verification")

1. Full `cargo test --workspace`; record final count in `AUDIT_CHECKLIST.md`
   in the existing phase-entry format, reproducing the wire-compat note from
   the parent doc's Context.
2. Vault updates (standing protocol): close `task-s5-3`/`task-s5-4`-era task
   nodes' successors for S6, bump `fact-test-suite` baseline, update
   `pillar-shielded-value-transfer` + `note-roadmap-status-*` to
   "S1.1–S6 landed", add one event node, keep `_system/INDEX.md` in sync,
   append one log node.
3. Report open questions to the roadmap owner: **#1 circuit audit (hard gate
   before real value)**, #3 proving UX (S6.4 numbers), #2 viewing-key policy,
   #4 anonymity-set bootstrap.

## Budget & escalation

Parent doc: S6 ≈ 46 h; S6.3 lands lighter (8/16-thread tests already exist).
**Stop-and-report** (no silent scope expansion) if S6.1 surfaces a
pipeline bug needing a non-test fix, or S6.4 exceeds the 100 ms verify
budget.
