---
id: log-organize-vault-20260923-134839
type: log
operation: organize-vault
date: 2026-09-23T13:48:39
namespace: pneumatic
summary: "Landed monolith-modularization step 4: node_server.rs (2393 ln) split into 5 concern modules + 6 test files per the committer d25f1ba template; main file now 183 ln; node-server 32/0/2 and workspace 823/32/0 green."
affected_nodes: ["concept-node-server-composite-runtime", "event-node-server-composite", "task-monolith-modularization", "fact-worker-crate-tests"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — node-server monolith split (step 4)

Refactor step 4 (task-monolith-modularization) landed, following the committer
d25f1ba template (per-concern modules + `tests/` subtree):

- `node-server/src/node_server.rs` 2393 → **183 ln**: imports, const action
  tables, `NodeServer` struct (L52-101), 7 kept methods (install/accessor set:
  `new`, `installed_roles`, `dispatch`, `selected_roles`, `shielded_pool`,
  `tokens`, `pending_registry`, `node_registry`), 3 re-export lines, 5 `pub mod`
  declarations, and the `#[cfg(test)] mod tests { pub mod helpers; + 5 domain
  mods }` declaration.
- New production modules under `node-server/src/node_server/` (each: doc header
  + `use super::*;`):
  - `build.rs` (271 ln): `build_runtime` (stays `pub`) + `load_stake_snapshot`.
  - `plugins.rs` (138 ln): `build_role_plugin` (17-arg, kept WHOLE — the S5.4
    single `Arc<ShieldedPool>` construction-site invariant).
  - `epoch_coord.rs` (60 ln): `current_epoch`, `roll_forward`,
    `recompute_role_set`, `poll_and_advance` (in `impl NodeServer`).
  - `transport.rs` (51 ln): `initiate_all_shutdown`, `spawn_coordinator` (in
    `impl NodeServer`) + free fn `route_data_plane` (kept OUTSIDE the impl
    block — a free fn, not a method).
  - `role_adapters.rs` (199 ln): the 8 `RoleHandler`/`RoleHost` impls
    (4 roles × 2 traits).
- Tests moved to `node-server/src/node_server/tests/` (6 files, pure moves):
  `helpers.rs` (520 ln — all 29 shared fixtures pub-ified + `pub use` re-export
  header; S5.4 live-composite fixtures: RecordingConnection, E2eDataProvider,
  ReducedFinalizerPlugin, MapStakeProvider), `build` (5), `transport` (5),
  `epoch` (3), `shielded` (2), `e2e` (2) = 17 test fns, exact baseline. Domain
  files open with `use super::helpers::*; use super::super::*;`.
- Visibility: 2 cross-sibling free fns promoted `fn` → `pub(crate) fn`
  (`build_role_plugin`, `route_data_plane`). NO `pub fn` shrank; the public API
  is unchanged because the parent re-exports the moved free fns:
  `pub use self::build::build_runtime;` (the bin-visible path survives) +
  `pub(crate) use self::plugins::build_role_plugin;` +
  `pub(crate) use self::transport::route_data_plane;`.
- First step where the template's `use super::*;` was NOT enough for sibling
  visibility: children don't see each other through the parent glob, so the
  parent carries the explicit re-exports (same mechanism as the finalizer's
  kept free fn).
- `super::role_selector` paths inside `build_runtime` retargeted
  `super::` → `crate::` (role_selector is a crate-root sibling; from a child
  module it is `super::super`, written as `crate::` for clarity) — 2 sites.
- Deviations from the task row: (a) deps.rs / RoleDeps DEFERRED — collapsing
  the 17-arg `build_role_plugin` signature is an API change, not a pure move;
  recorded as a follow-up; (b) one extra production module (role_adapters,
  holding the 8 handler/host impls) beyond the row's 4-name list; (c) the
  mod-header `use super::{...}` line kept as a PLAIN (non-pub) re-import with
  `#[allow(unused_imports)]` — helpers' `ReducedFinalizerPlugin` impls need
  `RoleHandler`/`RoleHost` in scope, and a `pub` re-export of `pub(crate)`/
  private names is E0364/E0365.
- Suite: node-server 32 passed / 0 failed / 2 ignored (exact baseline; the 2
  ignores are the S5.4 live-halo2 e2e tests, now in tests/e2e.rs); workspace
  823 passed / 32 ignored / 0 failed (exact baseline); zero new node-server
  warnings (`cargo check --workspace --all-targets` shows 0 in this crate).
- Line-reference updates: concept-node-server-composite-runtime (build_runtime
  → node_server/build.rs, build_role_plugin → node_server/plugins.rs,
  coordinator → node_server/{epoch_coord,transport}.rs; staleness signal now
  covers the node_server/ dir; verified_at bumped). event-node-server-composite
  and fact-worker-crate-tests checked — no stale line refs (event has none;
  fact's node-server count 32 + 2 ignored is unchanged).

Next: step 5 (root-lib splits: registry → validation → epoch → node/registry)
— **paused for user review** per standing instruction.
