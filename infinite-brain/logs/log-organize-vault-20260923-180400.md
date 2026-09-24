---
id: log-organize-vault-20260923-180400
type: log
operation: organize-vault
date: 2026-09-23T18:04:00
namespace: pneumatic
summary: "Landed monolith-modularization step 5 (final worker step): all four root-lib files split — registry.rs 1486→112, validation.rs 1992→47 (facade), epoch.rs 2064→51 (facade), node/registry.rs 2275→318 — per the committer d25f1ba template; core 541/0/19, workspace 823/32/0, warning fingerprint 122→122."
affected_nodes: ["task-monolith-modularization", "concept-validation-specs", "concept-transaction-lifecycle", "concept-nullifier", "concept-epoch-and-staking", "concept-leader-election", "concept-node-registry", "concept-pending-tx-registry", "pattern-fail-closed", "pattern-two-tier-snapshot-cache", "decision-nullifier-consensus-critical"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — root-lib splits (step 5)

Refactor step 5 (task-monolith-modularization) landed: the four root-lib
multi-concern files, executed in the planned order with a green
`cargo test -p pneumatic_core` (541 passed / 0 failed / 19 ignored — exact
baseline) after each one, then the full workspace at 823 passed / 32 ignored /
0 failed. Pure moves only; every public path (`crate::registry::*`,
`crate::validation::*`, `crate::epoch::*`, `crate::node::registry::*`) still
resolves through parent-module re-exports.

Shapes landed:

- `src/registry.rs` 1486→112 ln: the two registry structs + `TransactionPool`
  stay in the main file; `registry/{pending, nullifiers, signatures}` hold the
  impls; `registry/tests/` = 54 tests (nullifiers 12, pending 30, signatures 12)
  + 1 shared fixture.
- `src/validation.rs` 1992→47 ln pure facade: `validation/{traits, self_signed,
  executed, registries, shielded}`; 58 tests under `validation/tests/`
  (self_signed 11, executed 16, registries 5, shielded 26). The `registries`
  test file needs no helpers glob.
- `src/epoch.rs` 2064→51 ln pure facade (the task's 1860 count predates the
  step-1 EpochSnapshotCache): seven children `epoch/{types, stake_sets, leader,
  proposer, boundary, candidates, snapshot_cache}` — one more than the task's
  six, the snapshot cache moved with its 7 tests; 71 tests under `epoch/tests/`.
- `src/node/registry.rs` 2275→318 ln: the giant `impl NodeRegistry` (149–1031)
  split METHOD-level into `node/registry/{registration, heartbeat, fanout}`,
  each wrapping its methods in its own `impl NodeRegistry`; main keeps imports,
  `StakeCheck`, the struct, both consts, the free `bounded_send`/
  `bounded_send_async`/`record_delivery_failure`, `NullConnection`, `Drop`,
  `evict_expired`, init/eviction lifecycle, and the `#[cfg(test)]` `with_*`
  impl. Tests: 33 (registration 16, fanout 11, heartbeat 3, lifecycle 3) +
  13-item helpers (fixtures + the three test connections, fields pub-ified).

New lessons captured this step (beyond the sentinel/finalizer/node-server
conventions):

1. Promoted `pub(crate)` items living in a child are NOT in the parent
   namespace; tests reaching them through `super::super::*` need a parent
   re-export — and that re-export must be `#[cfg(test)] pub(crate) use` or the
   lib target mints a fresh unused-import warning.
2. Never `pub` a `use … as …` alias in the test helpers header: two globs
   offering the same name is a hard E0659 even when the items are identical.
3. A test fixture's absorbed module-header `use` lines must become `pub use`
   (except aliases) or their types go unresolved in domain test files (E0422).
4. Struct fields built via literals from sibling test files need
   struct-body-scoped pub-ification (the `HangingConnection`/
   `RecordingConnection` case).
5. Warning parity is judged per-message, per-file: the four unused imports in
   the pre-split validation facade died for free once children began
   referencing them via `use super::*`.

Vault sweep: nine nodes' `file:line` refs retargeted into the new module dirs
(concept-validation-specs, concept-transaction-lifecycle, concept-nullifier,
concept-epoch-and-staking, concept-leader-election, concept-node-registry,
concept-pending-tx-registry, pattern-fail-closed,
decision-nullifier-consensus-critical) + the two-tier-snapshot-cache pattern's
staleness signal; verified_at bumped 09/20→09/23; INDEX summaries kept in
sync. Task staleness_signal now reads "steps 1–5 DONE"; remaining steps:
6 (core C1 auth helper) and 7 (executor tests move).

Gates: `cargo check -p pneumatic_core --all-targets` 0 errors / 122 warnings
(pre-split 122, all survivors relocated); core suite 541/0/19; workspace
823/32/0; `no_threadpool_dependency` lib.rs gate still green (pure moves).
Changes left uncommitted for human review (HEAD `1d5d89f`).
