---
id: concept-shielded-pool-view
title: "ShieldedPoolView — read-only pool seam"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Trait (pool_view.rs:44) exposing pool state read-only to validation; SimpleShieldedPoolView (line 58) is the stand-in until S5.4 swaps in the real Arc<ShieldedPool>."
auto_inject: false
applicable_when: "Working on S5.3/S5.4, wiring the real pool, or adding pool-dependent checks"
confidence: 1.0
verified_at: "09/22/2026"
verified_by: "dsh-agent"
staleness_signal: "When S5.4 lands and the real ShieldedPool replaces the simple view"
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

`src/shielded/pool_view.rs` defines `ShieldedPoolView` (trait, line 44): a **read-only** interface over shielded pool state (nullifier membership + Merkle root history) that the validation spec consumes. Phase **S5.1** deliberately shipped this seam before the pool itself existed — the validation path can be built, tested, and wired end-to-end against `SimpleShieldedPoolView` (line 58), a static stand-in.

This is a standard **seam-first** tactic: define the contract, test against a stub, then swap in the real `Arc<ShieldedPool>` in **S5.4** without touching validation code.

**Post-S5.3 state (2026-09-22):** the real pool now `impl`s the trait (`committer/src/shielded_pool.rs:477`) and the composite composes the role views from it — `ShieldedPool::view_parts()` yields `(Arc<NullifierRegistry>, Arc<MerkleRootState>)`, which `SimpleShieldedPoolView::with` (pool_view.rs:80, 2-arg) wraps. Two semantics to keep straight:
- **The nullifier side is live** — the pool's `Arc<NullifierRegistry>` is the exact S4.2 registry the commit path marks/unmarks (`mark_many_atomic` on apply, `unmark_many` on rollback revert), so a role's view observes spent/nullifier state with no refresh.
- **The root side is a boot snapshot** — `view_parts()`/`view_roots` is the root state captured at pool construction; in safe Rust a `&'self dyn`-returning trait method cannot alias the guard's changing root state (no lock-free swap; crossbeam-epoch not added in S5.3). A role view therefore sees the root history as of pool boot. Making a role view track the *live* root is S5.4's design work (see the S5.4 plan's Decision 2 swap-site note).
