---
id: log-organize-vault-20260923-110500
type: log
operation: organize-vault
date: 2026-09-23T11:05:00
namespace: pneumatic
summary: "Landed monolith-modularization step 2: sentinel.rs (3337 ln) split into 5 concern modules + sentinel_error.rs + 7 test files per the committer d25f1ba template; main file now 179 ln; sentinel 57/1ig and workspace 823/32 green."
affected_nodes: ["fact-test-suite", "fact-worker-crate-tests", "task-monolith-modularization"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — sentinel monolith split (step 2)

Refactor step 2 (task-monolith-modularization) landed, following the committer
d25f1ba template (per-concern impl modules + `tests/` subtree + extracted
error type):

- `sentinel/src/sentinel.rs` 3337 → **179 ln**: imports, `Sentinel` struct,
  `new`, `initialize`, `on_data_received` (dispatch), `use crate::sentinel_error::SentinelError;`,
  5 `pub mod` declarations, `#[cfg(test)] mod tests { pub mod helpers; + 6 domain mods }`.
- New production modules under `sentinel/src/sentinel/` (each: doc header +
  `use super::*;` + `impl Sentinel { ... }`, private-field access preserved):
  - `processing.rs` (264 ln): handle_process_request, handle_self_signed,
    send_to_executor_for_preload, get_shard_executors, handle_clear_request,
    transition_to_failed.
  - `finalizing.rs` (128 ln): handle_confirmation, handle_rejection,
    assign_finalizer_deterministic(+_retry).
  - `registering.rs` (44 ln): handle_register_request, check_stake_for_type.
  - `epoching.rs` (127 ln): advance_epoch, handle_block_finalized_for_epoch.
  - `shielded.rs` (103 ln): handle_shielded_transfer (Phase S5.1 real path).
- `sentinel/src/sentinel_error.rs` (74 ln, `pub mod` in lib.rs): `SentinelError`
  enum + 4 From impls, verbatim move (its `super::transaction_notifier` paths
  resolve from crate-root level, unchanged).
- Tests moved to `sentinel/src/sentinel/tests/` (7 files, pure moves):
  `helpers.rs` (11 shared fixtures, all `pub fn` + `pub use` re-export header,
  committer convention), `processing` (27), `finalizing` (8), `registering` (5),
  `epoching` (4), `shielded` (23 incl. the `#[ignore]`d live end-to-end),
  `error` (2). Domain files open with `use super::helpers::*; use super::super::*;`.
  Cross-domain fixtures make_finalizing_entry + token_with_one_block live in helpers.
- Visibility: moved methods that are called across module lines (dispatch,
  cross-concern calls, tests) promoted `fn` → `pub(crate) fn` — the committer's
  exact convention (committer child modules are `pub(crate)` where the parent or
  tests reach them). No `pub fn` visibility shrank; crate public API unchanged.
- Main-file import cleanup: `ConnError`/`DataError` removed from sentinel.rs
  (consumers now have their own imports) — zero new warnings.
- Deviation from the task node's "9 test files" estimate: 7 files; the estimate
  was rough, every test has a home (counts above).
- Suite: sentinel 57 passed / 1 ignored (exact baseline); workspace
  823 passed / 32 ignored / 0 failed (exact baseline); `cargo check --workspace
  --all-targets` clean of new diagnostics.

Next: step 3 (finalizer split) — **paused for user review** per standing
instruction.
