---
id: log-organize-vault-20260922-132338
type: log
operation: organize-vault
date: 2026-09-22T13:23:38
namespace: pneumatic
summary: "Closed S5.3: ShieldedPool landed (single-writer guard, H12 + authoritative re-check, inverted apply-before-rollback ordering, fail-closed boot, 23 new tests incl. 5 live). AUDIT_CHECKLIST entry added; S5.4 plan patched to S5.3 actuals."
affected_nodes: ["task-s5-3-pool-append", "decision-global-shielded-pool", "concept-shielded-pool-view", "concept-incremental-merkle-tree"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — S5.3 close-out

S5.3 (committer `ShieldedPool`: pool append, persistence, nullifier commit) is implemented, verified, and closed.

**Implementation (all in-tree):** `committer/src/shielded_pool.rs` (new: `ShieldedPool` — one global instance, single `std::sync::Mutex<PoolState>`; `new`/`load` fail-closed/`from_state`/`rebuild_state`/`apply_update`/`revert_update`/`save`/`state_snapshot`/`view_parts`; `impl ShieldedPoolView` with live nullifiers + boot-snapshot roots; `pub fn nullifiers()` accessor added for cross-module tests). Commit path (`committer/committing.rs`): H12 wire-vs-registry `shielded` hash match → `shielded_commit_apply` (authoritative `ShieldedValidationSpec` re-verify with pool-owned deps; replay → `AlreadyApplied` BEFORE re-validation; atomic mark+append+root-push+delta under the one guard; on save failure the just-applied delta is reverted before `PoolPersist` surfaces — the chain never moves, so memory must not diverge) → save-if-`Applied` → `commit_block` → undo on chain-Err. Rollback branch inverts the ordering: winner applied+persisted first, then `commit_block(Some(loser))`, then the loser's delta reverted (`PoolRollback` = expected no-op for a buffered candidate). `handle_block_finalized` (finalizing.rs) applies+persists per committed block, idempotent, loud on failure. Boot: `committer/src/main.rs` + node-server `build_runtime` load via `ShieldedPool::load` (fail-closed) before BlockServices/Committer; node-server composes the role views from `view_parts()` (`SimpleShieldedPoolView::with`, 2-arg) — the S5.4 swap site. `build_runtime` now takes the data provider as a 3rd param (production keeps `DefaultDataProvider` + the strict fail-closed contract; the node-server tests inject an in-memory `MemoryDataProvider` that models a reachable service with no records ⇒ pristine genesis, since the fail-closed boot refuses to start against an unreachable store).

**Tests:** 23 new — 18 `shielded_pool.rs` unit tests, 5 fast committer-path discriminators (`committer/tests/pool.rs`), 5 `#[ignore]`d LIVE tests (real Halo2 prove, ~1 min each: advance+persist+reload; idempotent replay reapplies nothing; cross-block double-spend `StaleNullifier`; durability fail-save rolls back with no chain append; lockstep tip-rollback). Supporting: `TestDataProvider` gained an in-memory shielded-pool store + `with_shielded_save_failure` toggle; `make_shielded_block_for_token` helper (sets `signed_trans.shielded` and re-hashes); `IncrementalMerkleTree::commitment_to_leaf` made `pub` (core) so test mirror-deltas use the canonical commitment→leaf mapping.

**Patches:** `plans/S5.4-implementation-plan.md` rewritten against S5.3 actuals (load signature `load(&dyn DataProvider, &str, usize)`; the "one-line swap" is infeasible as a pure drop-in — `root_history()` is the boot-time snapshot in safe Rust, so S5.4 must first choose the live-view mechanism; `build_role_plugin` now has both view + pool params — the swap collapses it back to one; sentinel test arm cites the real 17-arg call site).

**Verified:** `cargo check --workspace --all-targets` clean; `cargo test -p pneumatic_committer` = 94 lib + 9 integration = 103 passed / 0 failed (5 live ignored); live suite run with `--ignored` green. `AUDIT_CHECKLIST.md` gained the Phase S5.3 section. `task-s5-3-pool-append` → closed; `decision-global-shielded-pool` and `concept-shielded-pool-view` updated to post-S5.3 state.
