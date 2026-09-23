---
id: task-s5-4-real-pool-swap
title: "S5.4 — Composite node-server: real Arc<ShieldedPool> wiring"
type: task
namespace: pneumatic
visibility: namespace
summary: "Replace the composite node's SimpleShieldedPoolView with the real shared Arc<ShieldedPool> in the node-server DI bundle, and add SignShielded/ShieldedVote to the finalizer role's action set."
auto_inject: false
applicable_when: "Implementing or reviewing S5.4, the final wiring phase of the shielded stack in the composite runtime"
confidence: 1.0
verified_at: "09/22/2026"
verified_by: "dsh-agent"
staleness_signal: "Closed — S5.4 entry landed in AUDIT_CHECKLIST.md (2026-09-22)"
tags: [task, shielded, node-server, pool, wiring, s5]
edges:
  - target: source-shielded-plan
    type: derived_from
    weight: 1.0
    note: "Specified in the S5.4 section (lines 905-941)"
  - target: concept-shielded-pool-view
    type: depends_on
    weight: 0.9
    note: "The stub view being swapped out for the real shared pool"
  - target: concept-shielded-pool
    type: depends_on
    weight: 0.8
    note: "The real pool (tree + nullifiers + applied-update map) becomes the shared artifact"
  - target: event-node-server-composite
    type: related_to
    weight: 0.8
    note: "The wiring lands inside the composite role-plugin runtime"
  - target: event-s5-2-finalizer-wiring
    type: related_to
    weight: 0.8
    note: "Adds the SignShielded/ShieldedVote finalizer actions S5.2 defined"
  - target: task-s5-3-pool-append
    type: related_to
    weight: 0.7
    note: "The committer write path it threads through presumes S5.3's guarded pool"
  - target: pillar-shielded-value-transfer
    type: part_of
    weight: 0.9
    note: "Completes the S5 wiring phase of the Tier-1 plan"
related: []
source_url: "repo:pneumatic-shielded-implementation-plan.md"
---

# Task: S5.4 node-server role wiring (real pool swap)

Specified in the shielded implementation plan, **S5.4 — Node-server role wiring** (lines 905–941).

Implementation plan: `plans/S5.4-implementation-plan.md` (09/20/2026). At planning time (09/20) the `FINALIZER_ACTIONS`/`handle` match arms already landed in S5.2 (node_server.rs:49–50, :664–697), so the diff was the `build_runtime` DI swap (node_server.rs:310–312) + `build_role_plugin` arm threading + tests. Hard prerequisite S5.3 landed 09/22/2026 the same day S5.4 closed.

**Status (CLOSED 2026-09-22 — done):** all deliverables landed in `node-server/src/node_server.rs`. (1) **View swap (Decision 2, option (a))**: the sentinel/finalizer arms now consume the SAME shared `Arc<ShieldedPool>` as their `ShieldedPoolView` — one `let shielded_pool_view: Arc<dyn ShieldedPoolView> = shielded_pool.clone() as Arc<dyn _>` at the `build_role_plugin` boundary; the `SimpleShieldedPoolView::with(pool.view_parts())` composition is GONE from the composite (zero references in `node-server/`); `build_role_plugin` keeps its single `shielded_pool` param (the separate view param is dropped). `SimpleShieldedPoolView` remains in `src/shielded/pool_view.rs` for split-deployment replay views; `view_parts()` stays public. (2) **Consensus-safety rationale** (documented in `committer/src/shielded_pool.rs` module docs): the role view's `root_history` keeps boot-snapshot semantics — safe because the advisory sentinel/finalizer gates only need "a recent valid root" while the commit-time AUTHORITATIVE re-check reads the pool's live roots under the single-writer guard, so a stale view can never admit a commit. (3) **Tests** (all in the node-server test module): `signshielded_removed_from_action_set_is_rejected_by_dispatcher` (fast discriminator — removing `SignShielded` from the finalizer action set ⇒ dispatcher rejects `UnknownAction`; the S5.2 action-set pin now has a regression test); `live_composite_shielded_e2e_all_four_hops_advance_the_pool` (`#[ignore]`, real prove ~1 min): all four roles in ONE composite node, a live-proven transfer (100 = 90 + 10) relayed through all four dispatch hops (`Verify` → `SignShielded` → `ShieldedVote` → `Commit`), the test driving ONLY the relays (sentinel self-registers + assigns the finalizer; finalizer self-builds + signs; committer materializes the pending entry from the authenticated wire block, H4) — terminal lockstep: shared pool leaf 1→2, applied 1→2, nullifier marked, `current_root()` == rebuilt tree root, chain tip == committed block; `live_composite_conflict_rollback_lockstep` (`#[ignore]`, two real proves ~2 min): siblings A (stake 100) / B (stake 200) proven against the shared final root, both chained off the same genesis tip — A commits, B wins the same-position conflict (unequal stakes ⇒ no TieFlagBoth), A rolled back LOCKSTEP through the composite's dispatch path (tip == B, chain genesis + B, applied == seed + B, A's nullifier unmarked, B's marked, `current_root()` == rebuilt tree root); finalizer-free by design (two hand-signed commits from two registered finalizer identities) so the committer's conflict path is the unit under test. Dev-deps added to `node-server/Cargo.toml`: `pneumatic_prover`, `pasta_curves = "=0.5.2"`, `async-trait = "0.1.88"` (all pinned to already-resolved versions — zero new lockfile packages). Suite: 847 passed / 16 ignored / 0 failed. One fix during bring-up: the e2e's data-provider token must be the SAME genesis-chained token as the composite's token cache (the finalizer's `resolve_previous_hash` reads the PROVIDER; a provider token without genesis fails the committer's `ChainLinkage` check).

Actions: (1) `FINALIZER_ACTIONS` (`node_server.rs:43`, currently `&["Sign", "Finalize"]`) gains `"SignShielded"` and `"ShieldedVote"`, with new `handle` match arms; the existing `"Sign"` arm and the fail-closed else arm are untouched. (2) `SENTINEL_ACTIONS`/`COMMITTER_ACTIONS` stay **unchanged** — shielded transfers ride the `Verify` envelope and shielded commits the existing `Commit`/`BlockFinalized` actions. (3) `build_runtime` (:204–402) constructs the `ShieldedPool` (S1.6 tree + S4.2 nullifier registry + applied-update map, under the single-writer guard) as a shared **`Arc`** in the DI bundle — replacing the `SimpleShieldedPoolView` stand-in — and `build_role_plugin` (:409–520) threads it into the Sentinel plugin (read-only), the Finalizer plugin (read-only, for the K-recency check), and the Committer/`BlockServices` (write path, S5.3). (4) `route_data_plane` needs no special-casing — shielded payloads are ordinary length-prefixed messages (~4.6 KB typical, well under the 16 MiB frame cap).

Verify: composite node routes a shielded tx end-to-end in the in-process pipeline test; unknown-action rejection preserved; discriminator: removing `"SignShielded"` from `FINALIZER_ACTIONS` in a test build → vote requests rejected `UnknownAction`, proving the gate is load-bearing.
