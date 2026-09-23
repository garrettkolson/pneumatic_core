---
id: log-organize-vault-20260922-213244
type: log
operation: organize-vault
date: 2026-09-22T21:32:44
namespace: pneumatic
summary: "Closed S5.4: composite role views are the shared Arc<ShieldedPool> itself (no composed view; SimpleShieldedPoolView demoted to split-deployment replay). AUDIT_CHECKLIST S5.4 section added; task node closed; pool-view concept rewritten to post-swap state; INDEX synced."
affected_nodes: ["task-s5-4-real-pool-swap", "concept-shielded-pool-view"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — S5.4 close-out

S5.4 (composite node-server: real pool swap) is implemented, verified, and closed.

**Implementation (all in `node-server/src/node_server.rs`):** at the `build_role_plugin` boundary the sentinel/finalizer arms consume the SAME shared `Arc<ShieldedPool>` as their `ShieldedPoolView` (`shielded_pool.clone() as Arc<dyn ShieldedPoolView>`; S5.4 plan Decision 2, option (a)). The `SimpleShieldedPoolView::with(pool.view_parts())` composition is gone from the composite build (zero references in `node-server/`); `build_role_plugin` keeps its single `shielded_pool` param. `SimpleShieldedPoolView`/`view_parts()` survive for split-deployment replay views. Consensus-safety rationale (boot-snapshot `root_history` is intentional; the commit-time authoritative re-check reads live roots under the guard) documented in `committer/src/shielded_pool.rs` module docs (stale S5.3-era "S5.4's design work" docs fixed).

**Tests (node-server test module; dev-deps `pneumatic_prover` + `pasta_curves = "=0.5.2"` + `async-trait = "0.1.88"`, zero new lockfile packages):** `signshielded_removed_from_action_set_is_rejected_by_dispatcher` (fast discriminator — the S5.2 action-set pin now has a regression test); `live_composite_shielded_e2e_all_four_hops_advance_the_pool` (`#[ignore]`, real prove): all four roles in ONE composite, a live-proven transfer (100 = 90 + 10) relayed through all four dispatch hops, the test driving ONLY the relays (sentinel self-registers + assigns the finalizer; committer materializes the pending entry from the authenticated wire block, H4) — terminal lockstep: leaf 1→2, applied 1→2, nullifier marked, `current_root()` == rebuilt tree root, chain tip == committed block; `live_composite_conflict_rollback_lockstep` (`#[ignore]`, two real proves): siblings A (stake 100) / B (stake 200) both chained off the same genesis tip — A commits, B wins the same-position conflict (unequal stakes), A rolled back lockstep (tip == B, applied == seed + B, A's nullifier unmarked, B's marked, `current_root()` == rebuilt tree root); finalizer-free by design (two hand-signed commits from two registered finalizer identities). Bring-up fix: the e2e data-provider token must be the SAME genesis-chained token as the composite's token cache (the finalizer's `resolve_previous_hash` reads the PROVIDER; a provider token without genesis fails the committer's `ChainLinkage` check).

**Verified:** `cargo check --workspace --all-targets` clean; `cargo test --workspace` = 847 passed / 16 ignored / 0 failed; both live tests green via `cargo test -p pneumatic_node_server -- --ignored`. `AUDIT_CHECKLIST.md` gained the Phase S5.4 section (S5.4.1–S5.4.5) and its two S5.3 forward-references updated to closed. `task-s5-4-real-pool-swap` → closed; `concept-shielded-pool-view` rewritten to post-swap state; `_system/INDEX.md` synced.
