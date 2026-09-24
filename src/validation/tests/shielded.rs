//! Shielded spec tests: fail-closed registration, structural checks 1-3,
//! nullifier + root-window freshness (incl. concrete-registry variants),
//! the K=10 verifier build, and the live-prove end-to-end case.
use super::helpers::*;
use super::super::*;

#[test]
fn register_shielded_adds_spec_without_touching_defaults() {
    // S4.1.3 discriminator: register_defaults() alone leaves no "Shielded" spec
    // (a shielded tx would then fall to the fail-closed default); register_shielded()
    // adds it and does not alter the existing defaults.
    let mut reg = ValidationSpecRegistry::new();
    reg.register_defaults();
    assert!(reg.get("Shielded").is_none(), "defaults must not register Shielded");

    reg.register_shielded();
    let spec = reg.get("Shielded").expect("register_shielded adds a Shielded spec");
    assert_eq!(spec.name(), "Shielded");

    // Defaults byte-identical.
    assert!(reg.get("SelfSigned").is_some());
    assert!(reg.get("Executed").is_some());
}

#[test]
fn unregistered_shielded_action_fails_closed() {
    // S4.1.6 discriminator: the trait default impl rejects shielded txs for any
    // non-shielded spec. Exercise it directly on a plain spec — a shielded tx
    // routed to the wrong spec must fail closed (UnsupportedAction), never accept.
    let spec = ExecutedBlockValidatorSpec::new(0);
    let tx = make_shielded_tx();
    let spent = FakeNullifier::default();
    let roots = FakeRootHistory::default();
    let env = make_env_with_defaults();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &roots, recency_window: 10 };
    let result = TransactionValidationSpec::validate_shielded(&spec, &tx, &env, &deps);
    assert!(reason_matches(&result, ValidationFailureReason::UnsupportedAction),
        "a non-shielded spec must fail closed on validate_shielded");
}

#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored proof_check_rejects_garbage_proof`"]
fn proof_check_rejects_garbage_proof() {
    // S4.1.4 discriminator: a valid-shape tx (checks 1-3 pass) with a garbage
    // proof must reach and fail check 4 as InvalidShieldedProof — proving check 4
    // is real. A structural failure on the same input, by contrast, fails check 1.
    let tx = make_shielded_tx();
    let spent = FakeNullifier::default();
    let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
    let result = run_shielded(&tx, &spent, &roots, 10);
    assert!(reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
        "a valid-shape tx with a garbage proof must reach and fail check 4");
}

#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored structural_ok_shape_passes_checks_1_through_3`"]
fn structural_ok_shape_passes_checks_1_through_3() {
    // Same input as above, but with a *valid* proof would pass checks 1-3; here
    // the garbage proof only matters once checks 1-3 pass, so the failure at
    // InvalidShieldedProof itself proves checks 1-3 all passed (revert any one and
    // this input returns InvalidCommitment / StaleNullifier / StaleMerkleRoot).
    let tx = make_shielded_tx();
    let spent = FakeNullifier::default();
    let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
    let result = run_shielded(&tx, &spent, &roots, 10);
    assert!(reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
        "structural + nullifier + root must all pass before the proof check");
    // Sanity: the same tx with a stale nullifier must fail check 2, not check 4.
    let spent2 = FakeNullifier { spent: tx.nullifiers.clone() };
    let result2 = run_shielded(&tx, &spent2, &roots, 10);
    assert!(reason_matches(&result2, ValidationFailureReason::StaleNullifier),
        "an already-spent nullifier must be caught at check 2, before the proof check");
}

#[test]
fn structural_rejects_empty_nullifier_vec() {
    // S4.1.3 discriminator: an empty nullifier vector fails check 1 (InvalidCommitment).
    let mut tx = make_shielded_tx();
    tx.nullifiers = vec![];
    let spent = FakeNullifier::default();
    let roots = FakeRootHistory::default();
    let result = run_shielded(&tx, &spent, &roots, 10);
    assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
        "an empty nullifier vector must fail the structural check");
}

#[test]
fn structural_rejects_empty_spent_commitments() {
    // S4.1.2/1.3 discriminator: the new spent_commitments field is load-bearing —
    // an empty vec fails check 1 (InvalidCommitment), which would otherwise be a
    // check-4 failure.
    let mut tx = make_shielded_tx();
    tx.spent_commitments = vec![];
    let spent = FakeNullifier::default();
    let roots = FakeRootHistory::default();
    let result = run_shielded(&tx, &spent, &roots, 10);
    assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
        "an empty spent_commitments vector must fail the structural check");
}

#[test]
fn structural_rejects_empty_commitments() {
    // S4.1.3 discriminator: an empty output commitment vector fails check 1.
    let mut tx = make_shielded_tx();
    tx.commitments = vec![];
    let spent = FakeNullifier::default();
    let roots = FakeRootHistory::default();
    let result = run_shielded(&tx, &spent, &roots, 10);
    assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
        "an empty commitments vector must fail the structural check");
}

#[test]
fn structural_rejects_nullifier_count_over_circuit_cap() {
    // S4.1.3 discriminator: a tx with more nullifiers than the v1 circuit cap
    // (1-in) fails check 1 (InvalidCommitment). This is the S4.1.3 cap-enforcement.
    let mut tx = make_shielded_tx();
    tx.nullifiers.push(tx.nullifiers[0]); // two identical entries → over the cap
    let spent = FakeNullifier::default();
    let roots = FakeRootHistory::default();
    let result = run_shielded(&tx, &spent, &roots, 10);
    assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
        "nullifiers over the v1 circuit cap must fail the structural check");
}

#[test]
fn structural_rejects_duplicate_nullifiers() {
    // S4.1.3 discriminator: duplicate nullifiers within a tx fail check 1.
    let mut tx = make_shielded_tx();
    tx.nullifiers.push(tx.nullifiers[0]);
    let spent = FakeNullifier::default();
    let roots = FakeRootHistory::default();
    let result = run_shielded(&tx, &spent, &roots, 10);
    assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
        "duplicate nullifiers must fail the structural check");
}

#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored nullifier_check_rejects_already_spent`"]
fn nullifier_check_rejects_already_spent() {
    // S4.1.5 discriminator: an already-spent nullifier fails check 2
    // (StaleNullifier) — before the proof check. Same tx, fresh nullifier set,
    // passes check 2 and reaches (and fails) check 4.
    let tx = make_shielded_tx();
    let nullifier = tx.nullifiers[0];
    let spent = FakeNullifier { spent: vec![nullifier] };
    let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
    let result = run_shielded(&tx, &spent, &roots, 10);
    assert!(reason_matches(&result, ValidationFailureReason::StaleNullifier),
        "an already-spent nullifier must be rejected at check 2");
    // Contrast: same tx against an empty set reaches the (garbage) proof check.
    let spent_fresh = FakeNullifier::default();
    let result2 = run_shielded(&tx, &spent_fresh, &roots, 10);
    assert!(reason_matches(&result2, ValidationFailureReason::InvalidShieldedProof),
        "with a fresh nullifier the tx reaches the proof check");
}

/// S4.2.4 seam: S4.1's check-2 discriminator re-run against the *concrete*
/// `NullifierRegistry` (S4.1 ran it against `FakeNullifier`; this proves the
/// `NullifierMembership` impl lines up on the real type).
#[test]
fn check2_against_concrete_nullifier_registry_rejects_spent() {
    use crate::registry::NullifierRegistry;
    let tx = make_shielded_tx();
    let registry = NullifierRegistry::new();
    registry.try_mark_spent(tx.nullifiers[0]).unwrap();
    let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
    let deps = ShieldedValidationDeps { spent: &registry, roots: &roots, recency_window: 10 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::StaleNullifier),
        "check 2 must fire StaleNullifier against the concrete registry"
    );
}

/// S4.2.4 seam: with the concrete registry holding the nullifier as *fresh*,
/// the same tx must clear checks 1-3 and reach (and fail) the check-4 proof
/// check — the real registry neither rejects a fresh spend nor leaks the
/// placeholder proof earlier than S4.1's fake did.
#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored check2_against_concrete_registry_fresh_reaches_proof_check`"]
fn check2_against_concrete_registry_fresh_reaches_proof_check() {
    use crate::registry::NullifierRegistry;
    let tx = make_shielded_tx();
    let registry = NullifierRegistry::new(); // fresh: nothing spent
    let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
    let deps = ShieldedValidationDeps { spent: &registry, roots: &roots, recency_window: 10 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
        "a fresh concrete registry must let the tx past check 2 to the proof check"
    );
}

#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored root_freshness_accepts_within_window_and_rejects_beyond`"]
fn root_freshness_accepts_within_window_and_rejects_beyond() {
    // S4.1.5 discriminator: the referenced root within the recency window passes
    // check 3 (reaching the proof check); the same root beyond the window is
    // rejected as StaleMerkleRoot — proving the window, not mere equality, is
    // the freshness gate.
    let tx = make_shielded_tx();
    let spent = FakeNullifier::default();

    // History with the tx root (R0) at height 0 and a newer tip (T) at height 1.
    let tip = RootSnapshot { root: [7u8; 32], height: 1 };
    let r0 = RootSnapshot { root: tx.merkle_root, height: 0 };
    let history = FakeRootHistory { roots: vec![r0, tip] };

    // window 1: R0 is 1 commit behind the tip → within window → passes check 3.
    let within = run_shielded(&tx, &spent, &history, 1);
    assert!(reason_matches(&within, ValidationFailureReason::InvalidShieldedProof),
        "a root within the window must pass the freshness check");

    // window 0: the SAME root, SAME history, only the window shrinks → 1 commit
    // behind > window 0 → StaleMerkleRoot.
    let beyond = run_shielded(&tx, &spent, &history, 0);
    assert!(reason_matches(&beyond, ValidationFailureReason::StaleMerkleRoot),
        "a root beyond the window must be rejected as stale");
}

/// S4.3.3 seam: S4.1's "current root accepted" on the concrete type.
/// State = genesis + one push of the tx's own root (tip, distance 0) →
/// passes check 3, reaches check 4. The whole set fails to compile
/// without the `impl MerkleRootHistory` — the seam is provably
/// load-bearing (S4.2.4 style).
#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored check3_concrete_root_state_current_root_accepted`"]
fn check3_concrete_root_state_current_root_accepted() {
    use crate::registry::NullifierRegistry;
    let tx = make_shielded_tx();
    let mut state = MerkleRootState::new(10);
    state.push(tx.merkle_root); // tip = the root the tx references
    let spent = NullifierRegistry::new(); // fresh: check 2 passes
    let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
        "the current tip root must pass check 3 and reach the proof check"
    );
}

/// S4.3.3 seam: the parent's "root from K-1 states back accepted", exact.
/// The tx references the genesis root (height 0); 9 dummy pushes put the
/// tip at height 9 → distance 9 = K-1 ≤ 10 → accepted, reaches check 4.
#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored check3_concrete_root_state_k_minus_1_back_accepted`"]
fn check3_concrete_root_state_k_minus_1_back_accepted() {
    use crate::registry::NullifierRegistry;
    let mut tx = make_shielded_tx();
    tx.merkle_root = [0u8; 32]; // the genesis zero root = the height-0 snapshot
    let mut state = MerkleRootState::new(10);
    for i in 1..=9u64 {
        state.push(dummy_root(i)); // tip at height 9
    }
    let spent = NullifierRegistry::new();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
        "a root K-1 commits behind the tip must pass check 3"
    );
}

/// S4.3.3 seam: the boundary discriminator. 10 dummy pushes → tip at
/// height 10, distance EXACTLY K. The root is still retained (capacity
/// window+1 = 11, nothing pruned), so the acceptance is decided purely
/// by the `<=` in `is_root_fresh` (validation.rs:522): an
/// implementation with `<` fails exactly this test. Together with
/// S4.1.5's fake test above (which pins the reject side of the same
/// comparison on a full history), the window math is pinned on both
/// sides.
#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored check3_concrete_root_state_at_window_boundary_accepted`"]
fn check3_concrete_root_state_at_window_boundary_accepted() {
    use crate::registry::NullifierRegistry;
    let mut tx = make_shielded_tx();
    tx.merkle_root = [0u8; 32];
    let mut state = MerkleRootState::new(10);
    for i in 1..=10u64 {
        state.push(dummy_root(i)); // tip at height 10
    }
    let spent = NullifierRegistry::new();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
        "a root exactly K commits behind the tip must pass check 3 (<=, not <)"
    );
}

/// S4.3.3 seam: the parent's "K+1 back rejected". 11 dummy pushes →
/// the 12th entry prunes the height-0 genesis root out → the referenced
/// root is absent. Per Decision 1, on the bounded state "beyond the
/// window" and "not found" are the same event; the assertion is the
/// parent's, the mechanism is the prune.
#[test]
fn check3_concrete_root_state_k_plus_1_back_rejected() {
    use crate::registry::NullifierRegistry;
    let mut tx = make_shielded_tx();
    tx.merkle_root = [0u8; 32];
    let mut state = MerkleRootState::new(10);
    for i in 1..=11u64 {
        state.push(dummy_root(i)); // tip at height 11, genesis root pruned
    }
    let spent = NullifierRegistry::new();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::StaleMerkleRoot),
        "a root K+1 commits behind the tip (pruned from the bounded state) must be rejected stale"
    );
}

/// S4.3.3 seam: the parent item's headline discriminator, verbatim
/// intent — "set K=0 in a test → the K-1 case now rejects (proves the
/// window logic, not just equality)". The tx root (the genesis seed)
/// WAS committed and IS in the history; it is rejected purely because
/// the window shrank to 0. Containment/`==` logic alone cannot explain
/// the outcome.
#[test]
fn check3_concrete_root_state_window_zero_rejects_one_back() {
    use crate::registry::NullifierRegistry;
    let mut tx = make_shielded_tx();
    tx.merkle_root = [0u8; 32];
    let mut state = MerkleRootState::new(0);
    state.push(dummy_root(1)); // tip at height 1 → the genesis root is 1 back
    let spent = NullifierRegistry::new();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 0 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::StaleMerkleRoot),
        "K=0 must reject a root even when it is in the committed history (window logic, not equality)"
    );
}

/// S4.3.3 seam: the accept half of K=0. No pushes — the genesis zero
/// root IS the tip (distance 0 ≤ 0) → passes check 3, reaches check 4.
/// Together with the previous test, pins "K=0 means exact tip only"
/// from both sides.
#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored check3_concrete_root_state_window_zero_accepts_exact_tip`"]
fn check3_concrete_root_state_window_zero_accepts_exact_tip() {
    use crate::registry::NullifierRegistry;
    let mut tx = make_shielded_tx();
    tx.merkle_root = [0u8; 32];
    let state = MerkleRootState::new(0); // genesis seed is the tip
    let spent = NullifierRegistry::new();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 0 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
        "K=0 must accept the exact tip"
    );
}

/// S4.3.3 seam: fail-closed on *unknown*, distinct from *stale*. The
/// tx references a root that is a perfectly valid field element but was
/// never committed — check 3 must reject it (a "accept if it looks
/// like a valid field element" implementation fails here).
#[test]
fn check3_concrete_root_state_unknown_root_rejected() {
    use crate::registry::NullifierRegistry;
    let mut tx = make_shielded_tx();
    tx.merkle_root = dummy_root(999); // decodable, never committed
    let mut state = MerkleRootState::new(10);
    state.push(dummy_root(1));
    let spent = NullifierRegistry::new();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::StaleMerkleRoot),
        "a never-committed (unknown) root must be rejected, not silently accepted"
    );
}

/// S4.3.3 seam: the boundedness assertion seen through the spec.
/// Window 2: push the tx root, then 3 dummies → the retained 3 entries
/// drop the tx root → `StaleMerkleRoot`, AND `state.len() == 3` in the
/// same test — the retention bound and its rejection consequence are
/// asserted together, so the orphan-buffer analogy is proven at the
/// validation boundary, not just at the type.
#[test]
fn check3_concrete_root_state_pruned_root_rejected() {
    use crate::registry::NullifierRegistry;
    let mut tx = make_shielded_tx();
    tx.merkle_root = dummy_root(1); // the tx root, committed at height 1
    let mut state = MerkleRootState::new(2);
    state.push(tx.merkle_root);
    state.push(dummy_root(2));
    state.push(dummy_root(3));
    state.push(dummy_root(4)); // retained: heights 2,3,4 — tx root gone
    let spent = NullifierRegistry::new();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 2 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::StaleMerkleRoot),
        "a root pruned from the bounded history must be rejected stale"
    );
    assert_eq!(state.len(), 3, "retention stays bounded to window + 1");
}

/// S4.3.4 — Decision 2's property end-to-end: the first transfer on a
/// fresh network. The tx references the empty pool's root (`[0u8; 32]`,
/// the genesis seed) and the state is `new(10)` with zero pushes — the
/// genesis snapshot alone must satisfy check 3.
///
/// **Discriminator:** a state constructed *without* the genesis seed
/// (the one-line revert of Decision 2) is empty, so `is_root_fresh`
/// takes the empty-history arm and this becomes `StaleMerkleRoot` —
/// the seed is proven necessary, not cosmetic. No bootstrap deadlock.
#[test]
#[ignore = "slow: reaches check 4, whose lazy SHIELDED_VALIDATOR_VERIFIER pays the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change the four shielded checks, is_root_fresh/MerkleRootState, NullifierRegistry, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored genesis_pool_state_accepts_first_transfer`"]
fn genesis_pool_state_accepts_first_transfer() {
    use crate::registry::NullifierRegistry;
    let mut tx = make_shielded_tx();
    tx.merkle_root = [0u8; 32]; // the empty pool root a wallet proves against pre-commits
    let state = MerkleRootState::new(10); // fresh network: genesis seed only, no pushes
    let spent = NullifierRegistry::new();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &state, recency_window: 10 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &make_env_with_defaults(), &deps);
    assert!(
        reason_matches(&result, ValidationFailureReason::InvalidShieldedProof),
        "the first transfer (referencing the genesis pool root) must pass check 3 and reach the proof check"
    );
}

#[test]
#[ignore = "slow: directly touches SHIELDED_VALIDATOR_VERIFIER, paying the one-time ActionCircuit keygen_vk (~2.5 min). AGENT: un-ignore and re-run when you change ShieldedVerifier, the VK cache, SHIELDED_VK_K, or the ActionCircuit: `cargo test -p pneumatic_core -- --ignored validator_verifier_is_built_at_k10`"]
fn validator_verifier_is_built_at_k10() {
    // S4.1.4 discriminator: the verifying key must be built at K=10 to match the
    // prover (S3.3 proves at K=10). A different width rejects every proof.
    assert_eq!(SHIELDED_VALIDATOR_VERIFIER.width(), 10);
}

#[test]
fn checks_run_in_structural_then_nullifier_order() {
    // S4.1.6 discriminator: a tx that is BOTH structurally invalid (empty
    // nullifier vec) AND would fail the nullifier check must fail at check 1
    // (InvalidCommitment), not check 2 — proving checks run in order 1→2→3→4.
    let mut tx = make_shielded_tx();
    tx.nullifiers = vec![]; // check 1 structural failure
    let spent = FakeNullifier { spent: vec![[0u8; 32]] }; // would also fail check 2
    let roots = FakeRootHistory::default();
    let result = run_shielded(&tx, &spent, &roots, 10);
    assert!(reason_matches(&result, ValidationFailureReason::InvalidCommitment),
        "the structural failure must win over the nullifier check (order 1 before 2)");
}

#[test]
fn risk_gate_rejects_shielded_above_max_risk() {
    // The shielded spec's neutral risk (0.30) is still gated by max_risk. With a
    // max_risk of 0.20 the neutral risk is rejected — same code path as the plain
    // spec tests above — proving the neutral risk flows through the gate.
    let tx = make_shielded_tx();
    let spent = FakeNullifier::default();
    let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
    let mut env = make_env_with_defaults();
    env.max_risk = 0.20;
    let deps = ShieldedValidationDeps { spent: &spent, roots: &roots, recency_window: 10 };
    let result = ShieldedValidationSpec::new().validate_shielded(&tx, &env, &deps);
    assert!(reason_matches(&result, ValidationFailureReason::RiskExceedsThreshold),
        "the neutral risk must still be rejected by the max_risk gate below its score");
}

// --- S4.1.6 end-to-end live prove (benchmark-only, #[ignore]d) ---

/// The ONE end-to-end live proof through `validate_shielded`. `#[ignore]`d per
/// the roadmap's "proving is benchmark-only" rule; run on demand with:
///
/// ```text
/// cargo test --workspace -p pneumatic_core -- --ignored validate_shielded_end_to_end_live_prove
/// ```
///
/// Produces a real Halo2 proof over a satisfiable Action circuit, assembles a
/// `ShieldedTransaction` whose reconstructed public inputs equal the circuit's,
/// then asserts `validate_shielded` returns `Ok` through all four checks, and
/// that flipping the proof yields `InvalidShieldedProof`. This proves checks 1-4
/// all pass together on a real proof and that check 4 is genuine (not vacuous).
#[test]
#[ignore]
fn validate_shielded_end_to_end_live_prove() {
    use halo2_proofs::pasta::EqAffine;
    use halo2_proofs::plonk::{create_proof, keygen_pk, keygen_vk};
    use halo2_proofs::poly::commitment::Params;
    use halo2_proofs::transcript::{Blake2bWrite, Challenge255};
    use rand::rngs::OsRng;

    // A satisfiable 1-in/1-out circuit with a matching tx fixture (fee 100 = 90 + 10).
    let note = ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) };
    let spend_key = [0xABu8; 32];
    let output_note = ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3), rcm: Fq::from(4) };
    let fee: u64 = 10;
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (merkle_root, merkle_proof) = tree.append(&commit(&note));
    let circuit = ActionCircuit::new(note.clone(), spend_key, merkle_proof, merkle_root, output_note.clone(), fee, DEFAULT_DEPTH);
    let public_inputs = circuit.public_inputs();

    // Prove it at K = 10 (must match the verifying key's width).
    let params: Params<EqAffine> = Params::new(10);
    let vk = keygen_vk(&params, &circuit).expect("keygen_vk");
    let pk = keygen_pk(&params, vk, &circuit).expect("keygen_pk");
    let instances_owned = ShieldedVerifier::instances_for(&public_inputs);
    let columns_per_proof: Vec<Vec<&[Fp]>> = instances_owned.iter().map(|cols| cols.iter().map(|c| c.as_slice()).collect::<Vec<&[Fp]>>()).collect();
    let proofs: Vec<&[&[Fp]]> = columns_per_proof.iter().map(|cols| cols.as_slice()).collect();
    let instances: &[&[&[Fp]]] = &proofs;
    let mut transcript = Blake2bWrite::<_, EqAffine, Challenge255<EqAffine>>::init(Vec::new());
    create_proof(&params, &pk, &[circuit.clone()], instances, &mut OsRng, &mut transcript).expect("create_proof on a satisfiable circuit");
    let proof = transcript.finalize();

    // Assemble the wire tx with the real proof and matching fee.
    let tx = make_shielded_tx_with(proof, fee);

    let spent = FakeNullifier { spent: vec![] };
    let roots = FakeRootHistory { roots: vec![RootSnapshot { root: tx.merkle_root, height: 0 }] };
    let env = make_env_with_defaults();
    let deps = ShieldedValidationDeps { spent: &spent, roots: &roots, recency_window: 10 };

    // (a) A real proof over a fresh nullifier and current root validates through
    //     all four checks.
    assert!(
        matches!(ShieldedValidationSpec::new().validate_shielded(&tx, &env, &deps), Ok(_)),
        "a real proof must validate through all four checks"
    );

    // (b) Flipping one proof byte makes check 4 reject — proving check 4 is
    //     genuine, not vacuous.
    let mut bad = tx.clone();
    if let Some(last) = bad.proof.last_mut() {
        *last ^= 0xff;
    }
    let bad_result = ShieldedValidationSpec::new().validate_shielded(&bad, &env, &deps);
    assert!(reason_matches(&bad_result, ValidationFailureReason::InvalidShieldedProof),
        "a tampered proof must be rejected as InvalidShieldedProof");
}
