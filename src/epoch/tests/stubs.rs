//! Epoch-stub contract tests (TASKS.md test-gap tail, 10/01/2026).
//!
//! The stubs are deliberately inert — staking persistence is still stubbed
//! (`StubStakingManager` logs ops without persisting; real persistence is
//! post-MVP work). These tests pin the contract the rest of the system is
//! allowed to rely on while that is true: the reconciler contributes nothing,
//! the staking manager accepts anything without failing, and both stay usable
//! as trait objects (the call sites hold `Box<dyn …>`/`Arc<dyn …>`).
use super::super::*;

#[test]
fn stub_reconciler_contributes_no_reconciliation_data() {
    let rec = StubEpochReconciler.reconcile();
    // The entire contract is emptiness: every vector default-empty.
    assert!(rec.misshapen_tokens.is_empty());
    assert!(rec.finalization_conflicts.is_empty());
    assert!(rec.slashing_ops.is_empty());
    assert!(rec.reward_ops.is_empty());
    // And it is exactly the Default form the committer's boot path assumes.
    let default = EpochReconciliation::default();
    assert_eq!(
        (
            default.misshapen_tokens.len(),
            default.finalization_conflicts.len(),
            default.slashing_ops.len(),
            default.reward_ops.len()
        ),
        (
            rec.misshapen_tokens.len(),
            rec.finalization_conflicts.len(),
            rec.slashing_ops.len(),
            rec.reward_ops.len()
        )
    );
}

#[test]
fn stub_staking_manager_accepts_any_ops_without_persisting() {
    // A fully-populated reconciliation must still be accepted verbatim: the
    // stub's ONLY contract is "log and return Ok" — a future refactor that
    // starts failing (instead of accepting) must update this test on purpose.
    let ops = EpochReconciliation {
        misshapen_tokens: vec![vec![1, 2, 3]],
        finalization_conflicts: vec![Conflict {
            block_a: vec![0xAA],
            block_b: vec![0xBB],
            stake_a: 10,
            stake_b: 5,
        }],
        slashing_ops: vec![StakingOp::Slash(vec![9], 100)],
        reward_ops: vec![StakingOp::Reward(vec![8], 1), StakingOp::AddStaker(vec![7], 50)],
    };
    let mgr = StubStakingManager;
    assert!(mgr.apply_ops(&ops).is_ok(), "stub must accept any ops");
    assert!(
        mgr.apply_ops(&EpochReconciliation::default()).is_ok(),
        "stub must accept the empty reconciliation too"
    );
}

#[test]
fn both_stubs_are_usable_as_trait_objects() {
    // The production call sites hold the traits as dyn handles; the stubs
    // must stay object-safe under `Box`/`Arc` (this fails to compile if a
    // future change breaks object safety).
    let reconciler: Box<dyn IEpochReconciler> = Box::new(StubEpochReconciler);
    let staking: std::sync::Arc<dyn IStakingManager> = std::sync::Arc::new(StubStakingManager);
    assert!(reconciler.reconcile().reward_ops.is_empty());
    assert!(staking.apply_ops(&EpochReconciliation::default()).is_ok());
}
