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
staleness_signal: "Stale once the individual splits land — each split should close its own slice of this list (step 1 DONE 09/23)"
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
| sentinel | sentinel.rs | 3333 (largest in repo) | 55 / ~2500 ln | Split: sentinel/{processing, finalizing, registering, epoching, shielded} + sentinel_error.rs + tests/ (9 files). Optional routing.rs service (finalizer assignment + shard select; dup prev-hash salt L403≈L697). Medium. |
| finalizer | finalizer.rs | 2373 | 25 / ~1414 ln | Split: finalizer/{signing, shielded, finalizing} + tests/{helpers, signing, shielded, epoch_stake}. Keep 4 existing siblings TOP-LEVEL. Optional: shielded_tx_store + unified voter_auth services; ~35 ln dup between try_finalize/_optimistic. Medium-low. |
| node-server | node_server.rs | 2393 | 17 / ~1548 ln | Split: node_server/{build, plugins, epoch_coord, transport} + tests/. New deps.rs (RoleDeps struct collapses 17-arg build_role_plugin) + role_adapters.rs. Keep build_role_plugin WHOLE (one construction site = S5.4 pool invariant). Medium. |
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
2. sentinel split
3. finalizer split
4. node-server split
5. root-lib splits (registry → validation → epoch → node/registry)
6. core C1 auth helper, batched per-crate
7. executor tests move (opportunistic)

Each step lands with its per-crate suite green; total ~4–6 focused days of mechanical moves.
