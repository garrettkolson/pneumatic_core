//! Phase S4.3 — bounded committed-root history for the merkle-root
//! freshness check.
//!
//! `MerkleRootState` is the concrete `MerkleRootHistory` (S4.1,
//! `validation.rs:413`) that check 3's `is_root_fresh` (`validation.rs:514`)
//! reads: the append-only history of committed pool roots, each with the
//! height at which it was produced. The pool root only advances as
//! commitments land in committed blocks (S5.3's guarded
//! `apply_pool_update`), and a shielded tx references the root it was
//! proved against (`ShieldedTransaction.merkle_root`, S3.1).
//!
//! The state is **bounded** — it retains exactly the newest
//! `window + 1` snapshots (the last K committed pool states plus the tip) —
//! mirroring the Phase 3.4 orphan buffer's tolerance for near-tip state.
//! That boundedness is safe *because* K is a consensus parameter (S5.3:
//! every node runs the same `shielded_root_recency`): any snapshot older
//! than distance K is rejected by the window arithmetic on *every* node, so
//! dropping it from retention cannot turn a reject into an accept anywhere.
//! See the type's doc comment for the full invariant set.
//!
//! This module is the in-memory read-side structure only: no persistence,
//! no external locks, no rewind API. S5.3's `ShieldedPool` owns the state
//! under its single-writer guard, rebuilds it from the applied-update map
//! at boot, and defines the rollback interaction (S4.3 plan, Decision 5).

use ff::Field;
use pasta_curves::pallas::Base as Fp;

use crate::shielded::tree::root_to_bytes;
use crate::validation::{MerkleRootHistory, RootSnapshot};

/// Bounded, append-only history of committed pool roots — the concrete
/// check-3 (merkle-root freshness) data structure.
///
/// # Invariants
///
/// - **Consensus-critical** (roadmap 2.5): every node's view of which
///   referenced roots are fresh must agree.
/// - **Append-only; the window prune is the sole removal.** `push` is the
///   only mutation; after each push the state retains exactly the newest
///   `window + 1` snapshots. There is deliberately **no rewind / rollback /
///   trim API of any kind** — the S5.3 tip-rollback interaction is an
///   open design question (S4.3 plan, Open item 3) and no method is
///   exposed for it, mirroring `NullifierRegistry`'s no-un-mark stance.
/// - **Consecutive snapshots have distinct roots.** Every applied pool
///   update appends ≥ 1 commitment, so the tree root strictly advances
///   (upstream guard: S4.1 check 1's non-empty, pairwise-distinct
///   commitments + S5.3's guarded apply). Distinctness keeps the
///   root→height lookup in `is_root_fresh` unambiguous within the window.
/// - **Heights are a 0-based pool-update sequence number, not chain block
///   heights.** The pool is global while per-token chains interleave
///   (S5.3); `push` returns the new tip's height.
/// - **Never empty.** `new` seeds the genesis pool state (the empty tree's
///   root, `root_to_bytes(&Fp::zero())`, at height 0) so the first
///   transfer on a fresh network has a valid reference target.
/// - **`Send + Sync`** (plain `Vec` + `usize`) so S5.3 can share the state
///   behind `Arc` exactly as it shares `NullifierRegistry`.
///
/// # Scope boundary (S5.3)
///
/// In-memory read-side state only: no persistence, no data-provider
/// `SaveOp`/`GetOp`, no lock acquisition, no load-at-boot. S5.3's
/// `ShieldedPool` wraps `Arc<MerkleRootState>` under its single-writer
/// guard, calls `push` with the post root inside `apply_pool_update`
/// (step 4, under the guard), rebuilds the state from the applied-update
/// map at boot (the history is a *derived, bounded view* — every snapshot
/// is "tree root after applying some prefix of the applied-update map"),
/// and fails closed on `current_root() != tree.root()`.
#[derive(Debug, Clone)]
pub struct MerkleRootState {
    /// The recency window this state retains (the consensus K, from the
    /// environment spec's `shielded_root_recency`).
    window: usize,
    /// The height the next `push` will assign — a monotonically increasing
    /// pool-update sequence counter. Heights must NOT be derived from
    /// `roots.len()`: once the window prune kicks in, the retained length
    /// is constant (`window + 1`) while the sequence keeps advancing.
    next_height: u64,
    /// Oldest → newest. At most `window + 1` entries, never empty
    /// (seeded with the genesis pool state at construction).
    roots: Vec<RootSnapshot>,
}

impl MerkleRootState {
    /// Create a state for recency window `k`, seeded with the genesis pool
    /// state: the empty tree's root (`Fp::zero()`) at height 0. A fresh
    /// network's first shielded transfer is proved against exactly this
    /// root, so the seed makes it verifiable on every node by construction.
    pub fn new(recency_window: usize) -> Self {
        Self {
            window: recency_window,
            // Genesis occupies height 0, so the first `push` is height 1.
            next_height: 1,
            roots: vec![RootSnapshot {
                root: root_to_bytes(&Fp::zero()),
                height: 0,
            }],
        }
    }

    /// Append a new committed pool root as the tip and prune the history to
    /// the newest `window + 1` entries (bounded retention — S4.3 plan,
    /// Decision 1). Returns the new tip's height.
    ///
    /// Contract: the caller must push a root distinct from the current tip
    /// — every applied pool update appends ≥ 1 commitment, so the root
    /// strictly advances. On the wire path that contract is enforced
    /// upstream (S4.1 check 1 + S5.3's guarded apply); this method is
    /// infallible by construction, not by validation.
    pub fn push(&mut self, root: [u8; 32]) -> u64 {
        // Height comes from the monotonic sequence counter, never from
        // `roots.len()`: after the first prune the retained length is
        // constant (`window + 1`) while the pool keeps advancing.
        let height = self.next_height;
        self.next_height += 1;
        self.roots.push(RootSnapshot { root, height });
        let keep = self.window + 1;
        if self.roots.len() > keep {
            self.roots.drain(..self.roots.len() - keep);
        }
        height
    }

    /// The current tip root. Never `None` — the genesis seed guarantees a
    /// non-empty history.
    pub fn current_root(&self) -> [u8; 32] {
        self.roots
            .last()
            .expect("genesis seed guarantees a non-empty root history")
            .root
    }

    /// The recency window this state was constructed with.
    pub fn recency_window(&self) -> usize {
        self.window
    }

    /// Number of retained snapshots (at most `window + 1`, at least 1).
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    /// Always `false` for this type (the genesis seed guarantees
    /// non-empty); present for read-side symmetry with
    /// `NullifierRegistry`.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

/// S4.1 check-3 seam (validation.rs:413): the committed-root history the
/// spec reads is exactly the retained snapshots, already in oldest→newest
/// order (the last element is the current tip).
impl MerkleRootHistory for MerkleRootState {
    fn root_history(&self) -> &[RootSnapshot] {
        &self.roots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::RootSnapshot;

    /// Build a distinct dummy root from a tag byte.
    fn dummy_root(tag: u8) -> [u8; 32] {
        let mut r = [0u8; 32];
        r[0] = tag;
        r
    }

    #[test]
    fn merkle_root_state_new_seeds_genesis_pool_state() {
        // Decision 2: the genesis entry is the empty tree's root
        // (Fp::zero() → 32 zero bytes) at height 0.
        let state = MerkleRootState::new(10);
        assert_eq!(state.len(), 1);
        assert!(!state.is_empty());
        assert_eq!(state.roots[0].root, [0u8; 32]);
        assert_eq!(state.roots[0].height, 0);
        assert_eq!(state.current_root(), [0u8; 32]);
        assert_eq!(state.current_root(), root_to_bytes(&Fp::zero()));
        assert_eq!(state.recency_window(), 10);
    }

    #[test]
    fn merkle_root_state_push_advances_height_and_current_root() {
        // Decision 1: heights are a 0-based pool-update sequence number.
        let mut state = MerkleRootState::new(10);
        let a = dummy_root(1);
        let b = dummy_root(2);
        assert_eq!(state.push(a), 1);
        assert_eq!(state.current_root(), a);
        assert_eq!(state.push(b), 2);
        assert_eq!(state.current_root(), b);
        assert_eq!(state.len(), 3);
        assert_eq!(state.roots[0].height, 0);
        assert_eq!(state.roots[1].height, 1);
        assert_eq!(state.roots[2].height, 2);
        assert_eq!(state.roots[2].root, b);
    }

    #[test]
    fn merkle_root_state_prunes_to_window_plus_one() {
        // Decision 1 (the orphan-buffer boundedness): after 5 pushes with
        // window 2, exactly the newest 3 snapshots survive.
        let mut state = MerkleRootState::new(2);
        for i in 0..5u8 {
            state.push(dummy_root(i + 1));
        }
        assert_eq!(state.len(), 3);
        assert_eq!(state.roots[0].height, 3);
        assert_eq!(state.roots[1].height, 4);
        assert_eq!(state.roots[2].height, 5);
        assert_eq!(state.roots[0].root, dummy_root(3));
        assert_eq!(state.current_root(), dummy_root(5));
    }

    #[test]
    fn merkle_root_state_history_oldest_to_newest() {
        // The trait contract (validation.rs:414): oldest→newest, last = tip.
        let mut state = MerkleRootState::new(10);
        for i in 0..4u8 {
            state.push(dummy_root(i + 1));
        }
        let heights: Vec<u64> = state.roots.iter().map(|s| s.height).collect();
        assert_eq!(heights, vec![0, 1, 2, 3, 4]);
        for w in heights.windows(2) {
            assert!(w[0] < w[1], "heights must be strictly ascending");
        }
        assert_eq!(state.roots.last().unwrap().root, state.current_root());
    }

    #[test]
    fn merkle_root_state_is_send_sync() {
        // Decision 1 / S5.3 contract: the state must be shareable behind
        // Arc (like NullifierRegistry). A future non-Send member breaks the
        // build loudly here.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MerkleRootState>();
    }
}
