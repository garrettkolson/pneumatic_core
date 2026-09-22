---
id: task-s5-3-pool-append
title: "S5.3 — Committer: pool append, persistence, nullifier commit"
type: task
namespace: pneumatic
visibility: namespace
summary: "Wire the committer to apply shielded pool updates at commit: authoritative proof re-check, idempotent replay, first-spent-wins StaleNullifier, K-freshness, atomic rollback, durable nullifiers."
auto_inject: false
applicable_when: "Implementing or reviewing the next shielded phase (S5.3) in the committer"
confidence: 1.0
verified_at: "09/22/2026"
verified_by: "dsh-agent"
staleness_signal: "Closed — S5.3 entry landed in AUDIT_CHECKLIST.md (2026-09-22)"
tags: [task, shielded, committer, pool, nullifier, s5]
edges:
  - target: source-shielded-plan
    type: derived_from
    weight: 1.0
    note: "Specified in the S5.3 section (lines 807-894)"
  - target: concept-nullifier
    type: depends_on
    weight: 0.9
    note: "Durable nullifier commit is the heart of the item"
  - target: concept-incremental-merkle-tree
    type: depends_on
    weight: 0.8
    note: "Appends commitment leaves under the single-writer guard"
  - target: concept-merkle-root-freshness
    type: depends_on
    weight: 0.7
    note: "Commit step 3 enforces the K-recency window"
  - target: pattern-fail-closed
    type: depends_on
    weight: 0.8
    note: "Boot with missing/corrupt pool state is an error, never start empty"
  - target: event-s5-2-finalizer-wiring
    type: related_to
    weight: 0.7
    note: "S5.2's votes bind the referenced (pre) root; the post root is computed here"
  - target: pillar-shielded-value-transfer
    type: part_of
    weight: 0.9
    note: "The next landing item of the Tier-1 shielded plan"
related: []
source_url: "repo:pneumatic-shielded-implementation-plan.md"
---

# Task: S5.3 committer pool append + nullifier commit

Specified in the shielded implementation plan, **S5.3 — Committer: pool state persistence + nullifier commit** (lines 807–894).

The pool is ONE global committer structure behind a **single-writer guard** (the Phase 5.4 single-epoch-writer pattern). It holds the Merkle tree (S1.6), the nullifier set (S4.2), and an **applied-update map** `(block_hash → {leaves appended, nullifiers marked})` that makes replay idempotent and rollback exact.

At commit, the guarded `apply_pool_update(block)` runs in order: (0) re-run `verify_shielded_proof` — the committer's own check is the final, authoritative consensus gate; (1) idempotent no-op if the block's hash is already in the map; (2) `StaleNullifier` reject if any nullifier was marked by a *different* block; (3) `referenced_root` within the K-recency window (K is a consensus parameter); (4) `mark_many_atomic` + append commitments + compute the post root + record the delta, atomically with the chain append.

**Status (CLOSED 2026-09-22 — done):** all deliverables landed. `ShieldedPool` (`committer/src/shielded_pool.rs`, ~1230 lines: `new`/`load`/`from_state`/`rebuild_state`/`apply_update`/`revert_update`/`save`/`state_snapshot`/`view_parts`; ONE global instance, single `std::sync::Mutex<PoolState>`, boot `view_roots` snapshot for the read-side view) is wired into the commit path: H12 wire-vs-registry `shielded` hash match (absent OR mismatch ⇒ `TransactionPayloadMismatch`), then `shielded_commit_apply` (authoritative re-verify via `ShieldedValidationSpec::new()` with pool-owned deps; idempotent replay short-circuits as `AlreadyApplied` BEFORE re-validation; atomic `mark_many_atomic` + leaf append + root push + delta record under the one guard) → save-if-`Applied` (durability: the nullifier record is written before the block is reported committed) → `commit_block` → on chain-Err `shielded_commit_undo` reverts the delta. The rollback branch (`commit_block(Some(loser))`) applies+persists the winner, then reverts the loser's delta — `PoolRollback` (loser had no delta: a buffered candidate) is the EXPECTED no-op, not an error. `handle_block_finalized` applies+persists per committed block (idempotent; failure logged + propagated). Save-failure closure: a durability `save` failure after a successful apply reverts the just-applied delta before `PoolPersist` surfaces (chain never moved ⇒ memory must not diverge). Core API additions: `IncrementalMerkleTree::membership_proof(index)` (re-derive an existing leaf's proof against the current root — an `append`'s proof is stale once later leaves land above it) and `commitment_to_leaf` made `pub`. Boot: `committer/src/main.rs` and `node-server` load the pool via `ShieldedPool::load` (fail-closed: missing key at the service is an error, never an empty pool) before BlockServices/Committer; node-server feeds the pool's `view_parts()` (the boot-time `Arc<NullifierRegistry>` + `Arc<MerkleRootState>`) into `SimpleShieldedPoolView::with` — S5.4 swaps that for `Arc<ShieldedPool>` directly. `build_runtime` takes the data provider as a 3rd param (production keeps `DefaultDataProvider`; tests inject the in-memory `MemoryDataProvider` — reachable service, no records ⇒ pristine genesis — because the fail-closed boot refuses to start against an unreachable store).

**Tests (all in `committer/src/committer/tests/pool.rs` + `shielded_pool.rs` unit tests, 23 new total):** 18 fast unit tests (load fail-closed/corrupt/divergent-root, rebuild roundtrip, apply rejects bad proof/stale nullifier/stale root, replay-`AlreadyApplied`-before-recheck, `revert_update_is_the_exact_inverse`, save fail-closed `PoolPersist`, view contract); 5 fast committer-path discriminators (H12 absent/swap, re-check bad proof/stale nullifier/stale root); 5 `#[ignore]`d LIVE tests (real Halo2 prove, ~1 min each): commit advances + persists + reloads identical, idempotent replay reapplies nothing (a failed chain append on a replay never reverts a previously applied delta), cross-block double-spend → `StaleNullifier`, durability fail-save rolls the delta back with no chain append, and lockstep rollback (winner applied+persisted, loser's delta reverted, pool and chain agree exactly).

**Hook sites (post-refactor; the planning doc's pre-refactor `committer.rs:432/461/519/527-535/672` refs are stale):** commit path in `committer/src/committer/committing.rs` — `check_and_commit_transaction_results` :48, H12 payload match :106, conflict→`block_services.commit_block` call sites :114–122; `handle_block_finalized` in `committer/src/committer/finalizing.rs:40`. Rollback unwinds pool state in lockstep with `blockchain.remove_block()`. Durability ordering: nullifiers are marked durable **before** the block is reported committed.
