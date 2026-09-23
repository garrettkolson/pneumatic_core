---
id: concept-shielded-pool-view
title: "ShieldedPoolView — read-only pool seam"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Trait (pool_view.rs:44) exposing pool state read-only to validation; S5.4 landed: the composite's role views are the shared Arc<ShieldedPool> itself (impl at shielded_pool.rs), SimpleShieldedPoolView survives only for split-deployment replay views."
auto_inject: false
applicable_when: "Wiring shielded roles, adding pool-dependent checks, or building split-deployment replay views"
confidence: 1.0
verified_at: "09/22/2026"
verified_by: "dsh-agent"
staleness_signal: "If a split-deployment replay view stops using SimpleShieldedPoolView, or the pool stops impl'ing the trait"
tags: [shielded, pool, seam, trait]
edges:
  - target: concept-shielded-pool
    type: supports
    weight: 1.0
    note: "The seam standing in for the real pool"
  - target: decision-global-shielded-pool
    type: derived_from
    weight: 0.9
    note: "Seam-first construction of the global-pool decision"
  - target: pillar-shielded-value-transfer
    type: part_of
    weight: 0.8
    note: "S5.1 deliverable"
related: []
source_url: "Empty"
---

# ShieldedPoolView — read-only pool seam

`src/shielded/pool_view.rs` defines `ShieldedPoolView` (trait, line 44): a **read-only** interface over shielded pool state (nullifier membership + Merkle root history) that the validation spec consumes. Phase **S5.1** deliberately shipped this seam before the pool itself existed — the validation path was built, tested, and wired end-to-end against `SimpleShieldedPoolView` (line 58), a static stand-in.

This was a standard **seam-first** tactic: define the contract, test against a stub, then swap in the real pool in **S5.4** without touching validation code.

**Post-S5.4 state (2026-09-22 — the swap is DONE):** the real pool `impl`s the trait (`committer/src/shielded_pool.rs`), and the composite passes the pool ITSELF as each shielded role's view — at the `build_role_plugin` boundary, `shielded_pool.clone() as Arc<dyn ShieldedPoolView>` (S5.4 plan, Decision 2 option (a)). There is NO separately composed view object in the composite build: the sentinel's/finalizer's view and the committer's pool are the SAME `Arc`, so the "single-pool invariant" is enforced by type-structure, not by convention. `SimpleShieldedPoolView` (pool_view.rs:58, 2-arg `with`) and `ShieldedPool::view_parts()` survive for **split-deployment replay views** that compose a view from an explicit snapshot — not for the composite.

Two semantics to keep straight (they apply to the pool's own trait impl now, not just the simple view):
- **The nullifier side is live** — the pool's `Arc<NullifierRegistry>` is the exact S4.2 registry the commit path marks/unmarks (`mark_many_atomic` on apply, `unmark_many` on rollback revert), so a role's view observes spent/nullifier state with no refresh.
- **The root side is a boot snapshot** — `view_roots`/`root_history` is the root state captured at pool construction; in safe Rust a `&'self dyn`-returning trait method cannot alias the guard's changing root state (no lock-free swap; crossbeam-epoch deliberately not added). This is INTENTIONAL and consensus-safe: the advisory sentinel/finalizer gates only need "a recent valid root" to re-verify against, while the commit decision is re-checked by the committer against the pool's own LIVE roots (read under the single-writer guard) — a stale view can never admit a commit (see `committer/src/shielded_pool.rs` module docs).
