---
id: task-monolith-modularization
title: "Refactor: apply the committer modularization pattern to the remaining monoliths"
type: task
namespace: pneumatic
visibility: namespace
summary: "Four worker-crate monoliths + four root-lib multi-concern files follow the pre-d25f1ba shape; plus two verified cross-crate duplications (epoch snapshot cache, C1 envelope auth). Prioritized, each step independently landable and test-gated."
auto_inject: false
applicable_when: "Planning or executing codebase refactors after the committer modularization (d25f1ba)"
confidence: 0.9
verified_at: "09/23/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale once the individual splits land — each split should close its own slice of this list (steps 1–4 DONE 09/23)"
tags: [task, refactor, modularization, code-health]
edges:
  - target: event-node-server-composite
    type: related_to
    weight: 0.6
    note: "node_server.rs grew 955 lines in S5.4 — the split must preserve the single Arc<ShieldedPool> construction site"
  - target: concept-committer-role
    type: derived_from
    weight: 0.8
    note: "d25f1ba's per-concern split + tests/ + extracted services is the template this task replicates"
  - target: pattern-authenticated-voter-c1
    type: related_to
    weight: 0.8
    note: "C1 auth duplicated at 5 production sites; a core authenticate_envelope helper would collapse it"
  - target: pattern-two-tier-snapshot-cache
    type: related_to
    weight: 0.9
    note: "sentinel + finalizer caches are byte-identical (code-only diff empty); generic EpochSnapshotCache<T> in core is the fix"
  - target: task-s5-3-pool-append
    type: related_to
    weight: 0.5
    note: "epoch.rs split is a good pre-step before S5.3-era staking persistence work lands there"
---

# Task: monolith modularization (committer pattern, applied to the rest)

Full analysis 09/23/2026 (four parallel deep-dives, line refs verified against code).
Template = commit d25f1ba: per-concern impl-split into a module dir via `use super::*;`,
tests moved to a per-concern `tests/` subtree (private-field access preserved),
side services + dedicated error type extracted.

## Worker-crate monoliths (direct analogs)

| Crate | File | Lines | Tests | Verdict |
|---|---|---|---|---|
| sentinel | sentinel.rs | 3333 (largest in repo) | 55 / ~2500 ln | ~~Split~~ **DONE 09/23/2026**: sentinel/{processing, finalizing, registering, epoching, shielded} + sentinel_error.rs + tests/ (7 files, not 9 — helpers/error merged into the domain set). Main file now 179 ln; suite green at 57/1ig (see log). |
| finalizer | finalizer.rs | 2379 | 61 (25 in monolith region + 36 in 4 siblings) | **DONE 09/23**: finalizer/{signing, shielded, finalizing} + tests/{helpers, signing, shielded, epoch_stake}. 4 existing siblings kept TOP-LEVEL (block_builder, message_dispatcher, signature_collector, lib). Deferred: shielded_tx_store + unified voter_auth services; ~35 ln dup between try_finalize/_optimistic. |
| node-server | node_server.rs | 2393 | 17 / ~1548 ln | ~~Split~~ **DONE 09/23/2026**: node_server/{build, plugins, epoch_coord, transport, role_adapters} + tests/{helpers, build, transport, epoch, shielded, e2e} (17 tests + 29 fixtures). Main file now 183 ln. `build_runtime`/`build_role_plugin`/`route_data_plane` re-exported at the parent so pre-split paths survive. Keep build_role_plugin WHOLE (one construction site = S5.4 pool invariant) — done. Deferred: deps.rs (RoleDeps struct collapsing the 17-arg build_role_plugin) — an API change, not a pure move; flagged for a follow-up. |
| executor | executor.rs | 1081 | ~500 ln | Cohesive — tests-only move at most. Low priority. |

## Root-lib multi-concern files (each 52–67% tests; low-risk pure moves)

1. `src/registry.rs` (1486) → registry/{mod, pending, nullifiers, signatures} — easiest
2. `src/validation.rs` (1992) → validation/{traits, self_signed, executed, shielded, registries}
3. `src/epoch.rs` (1860) → epoch/{types, stake_sets, leader, proposer, boundary, candidates}
4. `src/node/registry.rs` (2275) → node/registry/{mod, registration, heartbeat, fanout} — single struct, do last

## Cross-crate dedup (verified in code)

- **EpochSnapshotCache<T> in core (top priority):** sentinel/stake_snapshot_cache.rs (214) and
  finalizer/stake_snapshot_cache.rs (212) are code-identical (whitespace/comment-stripped diff empty);
  sentinel/executor_set_cache.rs (201) is a third copy differing only StakeSet→ExecutorSet.
  One generic ~120 ln core type (local Mutex<HashMap> tier + fetch hook + optional peer tier)
  replaces 3×~215.
- **authenticate_envelope core helper:** check_signature + find_node_types_by_public_key + role-gate
  at 5 production sites (finalizer.rs:290,444; committer.rs:304; block_services.rs:221;
  sentinel/transaction_notifier.rs:323). Core helper does envelope verify + role resolution
  (fail-closed); per-crate role-gate policy stays local. Medium effort — do after the moves.
- **NOT duplications (verified clean):** per-sender RMW lock (committer-only),
  ack/reject (core messages::acknowledge everywhere), shielded pool (core ShieldedPoolView trait since efcf963;
  concrete ShieldedPool correctly committer-owned).

## Suggested order

1. ~~core EpochSnapshotCache<T> + delete 3 worker copies~~ — **DONE 09/23/2026**: `pneumatic_core::epoch::EpochSnapshotCache<T>` (fetch-hook closure + std Mutex; 7 `epoch::tests::snapshot_cache_*` tests); sentinel/finalizer wire it in their constructors; 3 worker files deleted; suite green at 823/32 (see fact-test-suite delta arithmetic).
2. ~~sentinel split~~ — **DONE 09/23/2026**: 5 concern modules + sentinel_error.rs + 7 test files under `sentinel::tests`; main file 179 ln (struct + new + initialize + on_data_received + mod decls). Moved private methods promoted to `pub(crate)` (committer convention — cross-module calls from dispatch + tests); `pub` API unchanged. Deviation: 7 test files, not 9 (rough estimate in the original analysis; every test has a home: processing 27, finalizing 8, registering 5, epoching 4, shielded 23, error 2, helpers 11 fixtures). Workspace green 823/32, sentinel 57/1ig, no new warnings.
3. ~~finalizer split~~ — **DONE 09/23/2026**: 3 concern modules `finalizer/{signing, shielded, finalizing}` + 4 test files under `finalizer::tests` (helpers 1, signing 9, shielded 5, epoch_stake 10 = 25 moved; untouched siblings keep their 36 → crate total 61, exact baseline). Main file 278 ln (struct + new + initialize + dispatch + mod decls; `bytes_to_hex` free fn stays top-level, children reach it via `use super::*`). 5 cross-module methods promoted `pub(crate)`, incl. the multiline-`async fn` `try_finalize_optimistic` (signing→finalizing call). Deviations: (a) `make_stake_set` fixture cross-bucket (epoch_stake span, used by signing+shielded) → special-cased into helpers; (b) 4 imports (Block/Config/ReconciledSignatures/PendingTransaction + SignedTransaction/TransactionValidationResult in a second wave) pruned from the main file and re-exported from the tests helpers header (the sentinel convention, same reason); (c) `RecordingConnection` field `recorder` pub-ified (struct-body-scoped, not its same-named fn params). The task row's "25 tests" was stale — the real moved region is 48 test fns. Optional deferred items from the row: shielded_tx_store + unified voter_auth services, ~35 ln dup between `try_finalize`/`try_finalize_optimistic`. Workspace green 823/32/0, no new warnings (7 remaining, all pre-existing).
4. ~~node-server split~~ — **DONE 09/23/2026**: 5 concern modules `node_server/{build, plugins, epoch_coord, transport, role_adapters}` + 6 test files under `node_server::tests` (helpers = 29 shared fixtures pub-ified incl. the S5.4 e2e set; build 5, transport 5, epoch 3, shielded 2, e2e 2 = 17 moved, exact baseline). Main file 183 ln (struct + 7 kept accessor/lifecycle methods + mod decls). Free-fn moves needed parent re-exports (`pub use self::build::build_runtime;` for the bin-visible path; `pub(crate) use` for `build_role_plugin`/`route_data_plane` cross-sibling calls) — first step where the template's `use super::*;` wasn't enough for sibling visibility. Promotions: `build_role_plugin` + `route_data_plugin`→`route_data_plane` to `pub(crate)`; `super::role_selector` paths in build.rs retargeted to `crate::role_selector` (crate-root siblings are `super::super` from a child). Deviations from the row: (a) deps.rs/RoleDeps DEFERRED — collapsing the 17-arg signature is an API change, not a pure move; build_role_plugin kept whole per the S5.4 single-construction-site invariant; (b) one more prod module than the row's list (role_adapters, the 8 RoleHandler/RoleHost impls); (c) `use super::{...}` header line kept as a plain (non-pub) re-import with `#[allow(unused_imports)]` — helpers' fixture impls need `RoleHandler`/`RoleHost` in scope, a `pub` re-export is E0364/E0365. Workspace green 823/32/0, node-server 32/0/2 exact baseline, zero node-server warnings.
5. root-lib splits (registry → validation → epoch → node/registry)
6. core C1 auth helper, batched per-crate
7. executor tests move (opportunistic)

Each step lands with its per-crate suite green; total ~4–6 focused days of mechanical moves.
