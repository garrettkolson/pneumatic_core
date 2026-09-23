---
id: log-organize-vault-20260923-124440
type: log
operation: organize-vault
date: 2026-09-23T12:44:40
namespace: pneumatic
summary: "Landed monolith-modularization step 3: finalizer.rs (2379 ln) split into 3 concern modules + 4 test files per the committer d25f1ba template; main file now 278 ln; finalizer 61/0 and workspace 823/32 green."
affected_nodes: ["concept-finalizer-role", "pattern-authenticated-voter-c1", "task-monolith-modularization"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — finalizer monolith split (step 3)

Refactor step 3 (task-monolith-modularization) landed, following the committer
d25f1ba template (per-concern impl modules + `tests/` subtree):

- `finalizer/src/finalizer.rs` 2379 → **278 ln**: imports, `Finalizer` struct,
  `new`, `initialize` (unwired stub), dispatch, free fn `bytes_to_hex` (children
  reach it via `use super::*`), 3 `pub mod` declarations, and the
  `#[cfg(test)] mod tests { pub mod helpers; + 3 domain mods }` declaration.
- New production modules under `finalizer/src/finalizer/` (each: doc header +
  `use super::*;` + `impl Finalizer { ... }`, private-field access preserved):
  - `signing.rs` (164 ln): authenticate_signature_message,
    current_stake_for_voter, handle_preload, handle_signature.
  - `shielded.rs` (297 ln): authenticate_shielded_message,
    validate_shielded_advisory, handle_sign_shielded, handle_shielded_vote,
    try_finalize_shielded.
  - `finalizing.rs` (260 ln): get_stake_set_for_epoch, resolve_previous_hash,
    resolve_stake_metrics, try_finalize, try_finalize_optimistic.
- Tests moved to `finalizer/src/finalizer/tests/` (4 files, pure moves):
  `helpers.rs` (490 ln — all shared fixtures pub-ified + `pub use` re-export
  header), `signing` (9), `shielded` (5), `epoch_stake` (10). Domain files open
  with `use super::helpers::*; use super::super::*;`. The 4 pre-existing
  siblings (block_builder 6, message_dispatcher 8, signature_collector 22) keep
  their 36 tests top-level; 25 + 36 = 61, exact baseline.
- Visibility: 5 cross-module methods promoted `fn` → `pub(crate) fn`, incl.
  the multiline-signature `async fn try_finalize_optimistic` (called from
  signing's handle_signature). No `pub fn` visibility shrank; crate public API
  unchanged.
- Main-file import cleanup (sentinel convention, same reason): `Block`,
  `Config`, `ReconciledSignatures`, `PendingTransaction`, `SignedTransaction`,
  `TransactionValidationResult` pruned from finalizer.rs — the first four
  reached the moved tests via the parent glob, so `Block` and the transaction
  pair are re-exported from the tests helpers header (`pub use` lines) instead.
  Zero new warnings.
- Deviations: (a) `make_stake_set` (sits in the epoch_stake anchor span) is
  used by signing + shielded tests → special-cased into helpers; (b)
  `RecordingConnection` field `recorder` pub-ified — struct-body-scoped
  pubification, because a naive field pattern also matched same-named fn
  parameters; (c) the task row's "25 tests / ~1414 ln" was stale — the moved
  region is 48 test fns / ~1408 ln.
- Optional deferred items (not part of this step): shielded_tx_store +
  unified voter_auth services; ~35 ln duplication between `try_finalize` and
  `try_finalize_optimistic` (follow-up dedup candidate).
- Suite: finalizer 61 passed / 0 failed / 0 ignored (exact baseline, matches
  fact-worker-crate-tests); workspace 823 passed / 32 ignored / 0 failed
  (exact baseline); `cargo check --workspace --all-targets` clean — all 7
  remaining finalizer warnings are pre-existing (dead `try_finalize` method,
  unused `register_finalizer` fixture, etc.).
- Line-reference updates: concept-finalizer-role (handler ranges now point at
  finalizer/{signing,shielded,finalizing}.rs) and pattern-authenticated-voter-c1
  (C1 spans: signing.rs:113-153, shielded.rs:14-54, optimistic trigger
  signing.rs:150-155).

Next: step 4 (node-server split) — **paused for user review** per standing
instruction.
