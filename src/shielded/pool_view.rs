//! Phase S5.1 — the read-only shielded-pool seam every shielded role
//! depends on.
//!
//! The S4.1 shielded spec consumes two read sides of pool state as traits:
//! the spent-nullifier set (`NullifierMembership`, `validation.rs:407`) and
//! the committed-root history (`MerkleRootHistory`, `validation.rs:413`).
//! S4.1's own doc (`validation.rs:418-421`) promises the shape this module
//! realizes: "the composite node shares one `Arc<ShieldedPool>` (S5.3)
//! that implements both traits; split deployments replay block history
//! (S3.1 / S5.1) and expose it through the same traits."
//!
//! `ShieldedPoolView` is that seam. `SimpleShieldedPoolView` is the S5.1
//! **placeholder**: a fresh `NullifierRegistry` (S4.2) + `MerkleRootState`
//! (S4.3) pair, shared by `Arc`. It is *valid, pristine* state — any
//! non-genesis-root transfer validated against it fails closed at the
//! merkle-root freshness check — so a role constructed with it (before the
//! real pool exists) never accepts garbage.
//!
//! **Swap contract (S5.4):** S5.3's `Arc<ShieldedPool>` implements this
//! same trait, and S5.4's composite `build_runtime` changes exactly one
//! construction line to build it. The roles (sentinel here; finalizer in
//! S5.2) never change. A split-deployment replay view (S5.4/S6) implements
//! the same two methods over its replayed state — the seam is fixed now so
//! no later phase can reshape the roles' dependencies.
//!
//! The view is **read-only by construction**: it exposes no mutation path.
//! The pool's mutators (`try_mark_spent`, `push`, S5.3's guarded
//! `apply_pool_update`) stay with the pool that owns the state; the view
//! only hands out references to the shared `Arc`-ed structures.

use std::sync::Arc;

use crate::registry::NullifierRegistry;
use crate::shielded::roots::MerkleRootState;
use crate::validation::{MerkleRootHistory, NullifierMembership};

/// The read-only pool view the shielded roles' validation reads.
///
/// A view bundles the two read sides the S4.1 spec needs into one object, so
/// a role can never be handed the nullifier set and root history of *two
/// different pools* (the split-brain wiring the bundled type makes
/// unrepresentable). `Send + Sync` is pinned in the trait because the view
/// is shared across the role's threads behind `Arc` (S5.1 plan, Decision 1).
pub trait ShieldedPoolView: Send + Sync {
    /// The spent-nullifier set (S4.1 check 2).
    fn nullifier_set(&self) -> &dyn NullifierMembership;

    /// The committed-root history (S4.1 check 3).
    fn root_history(&self) -> &dyn MerkleRootHistory;
}

/// The S5.1 placeholder view: a fresh pair of the S4.2/S4.3 structures,
/// each shared by `Arc`. S5.4 swaps this construction site for the real
/// `Arc<ShieldedPool>`; nothing behind this type ever changes.
///
/// (No `Debug` derive: `NullifierRegistry` is a `DashMap` holder and is not
/// `Debug`; the view has no need for one.)
pub struct SimpleShieldedPoolView {
    nullifiers: Arc<NullifierRegistry>,
    roots: Arc<MerkleRootState>,
}

impl SimpleShieldedPoolView {
    /// A pristine view: an empty nullifier set and a genesis-seeded root
    /// state for recency window `k` (the environment's
    /// `shielded_root_recency`). Against this view, only a transfer proved
    /// against the genesis zero root is fresh — everything else fails
    /// closed at check 3.
    pub fn new(recency_window: usize) -> Self {
        Self::with(
            Arc::new(NullifierRegistry::new()),
            Arc::new(MerkleRootState::new(recency_window)),
        )
    }

    /// Build the view over pre-built, `Arc`-shared state — the
    /// split-deployment replay view's construction shape (S5.4/S6) and the
    /// test fixture's. The view is a live reference into the shared state,
    /// never a copy.
    pub fn with(
        nullifiers: Arc<NullifierRegistry>,
        roots: Arc<MerkleRootState>,
    ) -> Self {
        SimpleShieldedPoolView { nullifiers, roots }
    }
}

impl ShieldedPoolView for SimpleShieldedPoolView {
    fn nullifier_set(&self) -> &dyn NullifierMembership {
        &*self.nullifiers
    }

    fn root_history(&self) -> &dyn MerkleRootHistory {
        &*self.roots
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `new` seeds a valid, pristine view: the genesis snapshot at height 0
    /// and an empty nullifier set. This is the S5.1 plan's Decision 2
    /// property in miniature — a placeholder view fails closed on its own,
    /// with no special "no pool" branch.
    #[test]
    fn pool_view_new_seeds_genesis_and_empty_nullifiers() {
        let view = SimpleShieldedPoolView::new(10);

        let history = view.root_history().root_history();
        assert_eq!(history.len(), 1, "fresh view holds exactly the genesis snapshot");
        // `RootSnapshot` has no `PartialEq` derive, so compare fields.
        assert_eq!(history[0].root, [0u8; 32], "genesis root: the zero root");
        assert_eq!(history[0].height, 0, "genesis height");

        assert!(
            !view.nullifier_set().contains_nullifier([1u8; 32]),
            "fresh view's nullifier set is empty"
        );
    }

    /// The view is a **view**, not a copy: state advanced on the
    /// `Arc`-shared structures is visible through the view. A
    /// clone-at-construction implementation (snapshotting the sets into the
    /// view) fails exactly this test — and would break S5.3/S5.4, where the
    /// committer advances the same shared state the roles read.
    ///
    /// Two advance paths: the nullifier set is interior-mutable
    /// (`try_mark_spent` takes `&self`), so it advances *while the view
    /// exists* — the strongest liveness proof; the root state's `push`
    /// takes `&mut self`, so it is advanced from the owning handle while the
    /// `Arc` is unique (before the view clones it).
    #[test]
    fn pool_view_is_a_view_not_a_copy() {
        let nullifiers = Arc::new(NullifierRegistry::new());
        let mut roots = Arc::new(MerkleRootState::new(10));

        // Advance the root history from the owning handle (unique ref)…
        Arc::get_mut(&mut roots)
            .expect("unique while no view exists yet")
            .push([9u8; 32]);

        // …then build the view over the shared state.
        let view = SimpleShieldedPoolView::with(Arc::clone(&nullifiers), Arc::clone(&roots));

        // Advance the nullifier set *behind the live view* (`&self` interior
        // mutability)…
        nullifiers
            .try_mark_spent([7u8; 32])
            .expect("fresh registry: first mark succeeds");

        // …and the view must see all of it.
        assert!(
            view.nullifier_set().contains_nullifier([7u8; 32]),
            "the view sees nullifiers marked on the shared registry after the view exists"
        );
        let history = view.root_history().root_history();
        assert_eq!(
            history.last().map(|s| s.root),
            Some([9u8; 32]),
            "the view sees the root pushed on the shared state"
        );
        assert_eq!(history.len(), 2, "genesis + the pushed root");
    }

    /// Compile-time proof the trait bound is real: `Arc<dyn ShieldedPoolView>`
    /// is well-formed for the role constructors that take it. A future
    /// non-`Send`/non-`Sync` member of the view breaks the build loudly.
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn pool_view_is_send_sync() {
        assert_send_sync::<SimpleShieldedPoolView>();
        assert_send_sync::<Arc<dyn ShieldedPoolView>>();
    }
}
