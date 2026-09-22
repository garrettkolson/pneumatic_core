//! Phase S5.3 — the committer's global shielded pool: the sole owner of
//! committed shielded state (spent nullifiers + committed commitment leaves +
//! bounded root history), its durable boot/reload state, and the
//! single-writer apply/rollback path that keeps pool and chain in lockstep.
//!
//! # Design (S5.3 plan, decision "one global pool")
//!
//! Every committer (standalone and composite) owns exactly ONE `ShieldedPool`,
//! built once at boot and shared by `Arc`:
//!
//! - `nullifiers: Arc<NullifierRegistry>` — the same object the shielded
//!   roles' validation views read, shared *as-is*: every role and the pool
//!   see the exact same spent set (the "single-pool invariant" — a split
//!   nullifier view is the double-spend hole this exists to close).
//! - The merkle tree, the applied-delta map, and `Arc<MerkleRootState>` live
//!   under a single `std::sync::Mutex` (the plan's "single-writer guard").
//!   No other component mutates this state; the guard is acquired only by the
//!   commit-path and BlockFinalized-path pool operations below.
//!
//! # Why `std::sync::Mutex`
//!
//! The guarded operations are blocking (data-service persistence, Halo2 proof
//! verification) and the commit handlers already run blocking data-service
//! calls inside async tasks (the committer's established pattern — see the
//! `rmw_locks` field). A `tokio::sync::Mutex` would yield at the proof
//! verification mid-check, reordering concurrent shielded commits
//! non-deterministically; a `std` mutex serializes them into a total order,
//! which is what makes the pool's state deterministic across nodes.
//!
//! # The view's `root_history` is a boot-time snapshot
//!
//! `impl ShieldedPoolView for ShieldedPool` (below) returns references to
//! `nullifiers` and to a *stable* root state captured at pool construction.
//! In safe Rust a `&'self dyn`-returning trait method cannot point at a
//! changing snapshot: the guard's live `Arc<MerkleRootState>` is an
//! interior-mutable value, and no lock-free public swap exists for it
//! (crossbeam-epoch is deliberately not added in S5.3 — no lock-free reader
//! need exists until S5.4's role-view design). The live snapshot is reachable
//! *owned* via [`ShieldedPool::view_parts`], which is exactly what the
//! composite build uses to compose the sentinel's/finalizer's
//! `SimpleShieldedPoolView`. Making a role's view track the pool's *live*
//! root state is S5.4's design work (see the S5.4 plan's swap-site note).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pneumatic_core::blocks::{Block, BlockFactory};
use pneumatic_core::data::{AppliedPoolDelta, DataProvider, ShieldedPoolState};
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::registry::NullifierRegistry;
use pneumatic_core::shielded::{
    bytes_to_root, commitment_leaf, root_to_bytes, IncrementalMerkleTree, MerkleRootState,
    ShieldedPoolView, DEFAULT_DEPTH,
};
use pneumatic_core::validation::{
    MerkleRootHistory, NullifierMembership, ShieldedValidationDeps, ShieldedValidationSpec,
    TransactionValidationSpec,
};
use pasta_curves::pallas::Base as Fp;

use crate::committer_error::CommitterError;

/// Convert bytes to a lowercase hex string (no prefix).
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// The outcome of [`ShieldedPool::apply_update`] — tells the commit path
/// whether it applied a *new* delta (and so must persist / must undo on a
/// failed chain append), merely replayed an already-applied block (idempotent
/// no-op), or touched the pool at all (plain block).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolApplyOutcome {
    /// The block carried a shielded payload and its delta was newly applied.
    Applied,
    /// The block's hash is already in the applied map — an idempotent replay;
    /// no state changed and nothing needs persisting or undoing.
    AlreadyApplied,
    /// The block carried no shielded payload; the pool is untouched.
    Plain,
}

/// The committer's global shielded pool (module docs for the design).
pub struct ShieldedPool {
    // `Debug` is implemented manually below (the guard's contents are large).
    /// The shared spent-nullifier set (S4.2) — the SAME `Arc` the shielded
    /// roles' validation views read.
    nullifiers: Arc<NullifierRegistry>,
    /// The recency window (the consensus K) this pool's root state retains.
    recency_window: usize,
    /// The `ShieldedPoolView::root_history` target: the root state captured
    /// at pool construction. See the module docs for why the trait impl
    /// points here rather than at the guard's live snapshot (S5.4 designs
    /// the live role view).
    view_roots: Arc<MerkleRootState>,
    /// The single-writer state (module docs: the guard contract).
    guard: Mutex<PoolState>,
}

impl std::fmt::Debug for ShieldedPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShieldedPool")
            .field("recency_window", &self.recency_window)
            .field("leaf_count", &self.leaf_count())
            .field("applied_count", &self.applied_count())
            .finish()
    }
}

/// The guarded pool state: the tree and its ordered leaf sequence, the
/// committed-root history, and the applied-delta history (the pool's
/// complete, ordered, persistable record).
struct PoolState {
    /// The full ordered leaf sequence (the persistence source; the tree is a
    /// derived view of it).
    leaves: Vec<Fp>,
    /// The incremental merkle tree over `leaves`.
    tree: IncrementalMerkleTree,
    /// The committed-root history (S4.1 check 3), advanced per applied delta.
    roots: Arc<MerkleRootState>,
    /// The applied deltas in commit order — the pool's complete history.
    applied: Vec<AppliedPoolDelta>,
    /// `block_hash → index into `applied`` (the idempotency key).
    applied_index: HashMap<Vec<u8>, usize>,
}

impl ShieldedPool {
    /// A pristine pool: an empty tree (the genesis zero root) and an empty
    /// nullifier set, for recency window `k`.
    pub fn new(recency_window: usize) -> Self {
        let roots = Arc::new(MerkleRootState::new(recency_window));
        Self {
            nullifiers: Arc::new(NullifierRegistry::new()),
            recency_window,
            view_roots: Arc::clone(&roots),
            guard: Mutex::new(PoolState {
                leaves: Vec::new(),
                tree: IncrementalMerkleTree::new(DEFAULT_DEPTH),
                roots,
                applied: Vec::new(),
                applied_index: HashMap::new(),
            }),
        }
    }

    /// Load the pool at boot (the fail-closed boot contract):
    ///
    /// - the provider errors (`Err`) ⇒ `PoolState { kind: "corrupt" }` — a
    ///   read failure is never treated as "no state" (re-seeding would forget
    ///   prior spends);
    /// - `Ok(None)` (a true absence — fresh network / no shielded traffic) ⇒
    ///   a pristine genesis pool, **persisted immediately** so the next boot
    ///   has a verifiable state to load;
    /// - `Ok(Some(state))` ⇒ the state is rebuilt exactly (tree from the leaf
    ///   sequence, root history from the delta sequence, nullifiers from the
    ///   delta union) and every internal consistency is asserted
    ///   (`PoolState { kind: "integrity" | "invalid_root" }` on any fault).
    ///
    /// (The provider verifies the SHA-256 envelope fingerprint before
    /// returning the payload; everything here is payload-level integrity.)
    pub fn load(
        provider: &dyn DataProvider,
        partition_id: &str,
        recency_window: usize,
    ) -> Result<Arc<Self>, CommitterError> {
        let stored = provider
            .get_shielded_pool(partition_id)
            .map_err(|e| CommitterError::PoolState {
                kind: "corrupt",
                cause: format!(
                    "get_shielded_pool({partition_id}) failed: {e:?} — refusing to re-seed \
                     (re-seeding would forget prior spends)"
                ),
            })?;
        let pool = match stored {
            None => {
                let pool = Self::new(recency_window);
                // Persist the pristine seed so boot #2 loads a verifiable
                // state instead of re-deriving "absence" (the roadmap 2.5
                // durable record starts at genesis).
                pool.save(provider, partition_id)?;
                pool
            }
            Some(state) => Self::from_state(&state, recency_window)?,
        };
        Ok(Arc::new(pool))
    }

    /// Rebuild a pool from persisted state, asserting every internal
    /// consistency (module: `load`'s `Ok(Some)` arm).
    fn from_state(state: &ShieldedPoolState, recency_window: usize) -> Result<Self, CommitterError> {
        if state.leaf_count != state.leaves.len() as u64 {
            return Err(CommitterError::PoolState {
                kind: "integrity",
                cause: format!(
                    "leaf_count {} != leaves.len() {}",
                    state.leaf_count,
                    state.leaves.len()
                ),
            });
        }
        // Rebuild the tree + root history from the delta sequence, verifying
        // every recorded `post_root` against the recomputed value.
        let rebuilt = rebuild_state(&state.applied, recency_window, /*verify_recorded=*/ true)?;
        // The rebuilt leaf sequence must equal the stored one exactly.
        let rebuilt_leaves: Vec<[u8; 32]> =
            rebuilt.leaves.iter().map(|l| root_to_bytes(l)).collect();
        if rebuilt_leaves != state.leaves {
            return Err(CommitterError::PoolState {
                kind: "integrity",
                cause: "rebuilt leaf sequence != stored leaves".to_string(),
            });
        }
        // The rebuilt root history's tip must equal the recorded root.
        if rebuilt.roots.current_root() != state.root {
            return Err(CommitterError::PoolState {
                kind: "integrity",
                cause: format!(
                    "rebuilt root-history tip {} != recorded root {}",
                    bytes_to_hex(&rebuilt.roots.current_root()),
                    bytes_to_hex(&state.root)
                ),
            });
        }
        // The stored nullifier set must equal the union of the applied
        // deltas' nullifiers (the documented derivation), with no duplicates
        // (a duplicate means two blocks spent one nullifier — impossible from
        // the apply path, hence corruption).
        let mut union: Vec<[u8; 32]> = state
            .applied
            .iter()
            .flat_map(|d| d.nullifiers.iter().copied())
            .collect();
        let mut stored_nullifiers = state.nullifiers.clone();
        union.sort_unstable();
        stored_nullifiers.sort_unstable();
        if union != stored_nullifiers {
            return Err(CommitterError::PoolState {
                kind: "integrity",
                cause: "stored nullifier set != union of the applied deltas' nullifiers".to_string(),
            });
        }
        let nullifiers = Arc::new(NullifierRegistry::new());
        for delta in &state.applied {
            nullifiers
                .mark_many_atomic(&delta.nullifiers)
                .map_err(|e| CommitterError::PoolState {
                    kind: "integrity",
                    cause: format!(
                        "delta for block {} double-spends a nullifier: {e:?}",
                        bytes_to_hex(&delta.block_hash)
                    ),
                })?;
        }
        Ok(Self {
            nullifiers,
            recency_window,
            view_roots: Arc::clone(&rebuilt.roots),
            guard: Mutex::new(rebuilt),
        })
    }

    /// Apply a committed block's shielded delta to the pool — the S5.3
    /// single-writer path (roadmap 2.5):
    ///
    /// 0. the authoritative re-check: `ShieldedValidationSpec::validate_shielded`
    ///    (S4.1 checks 1→4) run against the pool's OWN state (its nullifier
    ///    set, its root history) — this node's verdict stands on its own
    ///    state; soundness never depends on the sentinel/finalizer views;
    /// 1. idempotent replay: a block whose hash is already applied is a
    ///    no-op, NOT a double-spend (the double-spend guard is the nullifier
    ///    set, which the re-check consults);
    /// 2. mark the nullifiers spent (`mark_many_atomic` — all-or-nothing; the
    ///    atomic gate against a concurrent spender);
    /// 3. append each output commitment's leaf (commitment → Fp → tree);
    /// 4. advance the root history and record the delta (the idempotency
    ///    index + the persistable history).
    ///
    /// Plain blocks (no shielded payload) return `PoolApplyOutcome::Plain`
    /// without touching state. The whole operation runs under the single
    /// guard (the re-check included — see the module docs on ordering).
    pub fn apply_update(
        &self,
        block: &Block,
        env_data: &EnvironmentMetadata,
    ) -> Result<PoolApplyOutcome, CommitterError> {
        let stx = match &block.signed_trans.shielded {
            Some(stx) => stx,
            None => return Ok(PoolApplyOutcome::Plain),
        };
        // The idempotency key: the block's own hash. The finalizer sets
        // `current_hash` before sending; a wire block that arrives without it
        // is derived canonically — the same value `Token::commit_block`
        // computes on the append path, so the key matches across paths.
        let block_hash = if block.current_hash.is_empty() {
            BlockFactory::create_hash(block).unwrap_or_default()
        } else {
            block.current_hash.clone()
        };
        let tx_id = stx.id.clone();

        let mut state = self.guard.lock().unwrap_or_else(|p| p.into_inner());

        // (1) Idempotent replay.
        if state.applied_index.contains_key(&block_hash) {
            return Ok(PoolApplyOutcome::AlreadyApplied);
        }

        // (0) The authoritative re-check against the pool's own state.
        let deps = ShieldedValidationDeps {
            spent: &*self.nullifiers,
            roots: &*state.roots,
            recency_window: self.recency_window,
        };
        ShieldedValidationSpec::new()
            .validate_shielded(stx, env_data, &deps)
            .map_err(|e| CommitterError::ShieldedProofInvalid {
                tx_id: tx_id.clone(),
                cause: format!("{e:?}"),
            })?;

        // (2) Spend the nullifiers (all-or-nothing).
        self.nullifiers
            .mark_many_atomic(&stx.nullifiers)
            .map_err(|e| CommitterError::ShieldedProofInvalid {
                tx_id: tx_id.clone(),
                cause: format!("nullifier mark failed after re-check: {e:?}"),
            })?;

        // (3) Append the output commitments' leaves.
        let mut post_leaves: Vec<[u8; 32]> = Vec::with_capacity(stx.commitments.len());
        for commitment in &stx.commitments {
            let leaf = commitment_leaf(commitment).map_err(|e| CommitterError::ShieldedProofInvalid {
                tx_id: tx_id.clone(),
                cause: format!("commitment leaf decode failed: {e:?}"),
            })?;
            state.tree.append_leaf(&leaf);
            state.leaves.push(leaf);
            post_leaves.push(root_to_bytes(&leaf));
        }

        // (4) Advance the root history; record + index the delta.
        let post_root = root_to_bytes(&state.tree.root());
        let mut staged = (*state.roots).clone();
        staged.push(post_root);
        state.roots = Arc::new(staged);
        let delta_index = state.applied.len();
        state.applied_index.insert(block_hash.clone(), delta_index);
        state.applied.push(AppliedPoolDelta {
            block_hash,
            leaves: post_leaves,
            nullifiers: stx.nullifiers.clone(),
            post_root,
        });
        Ok(PoolApplyOutcome::Applied)
    }

    /// Revert a previously applied block's delta — the S5.3 single-writer
    /// rollback (the exact inverse of `apply_update`), used by the
    /// commit-path lockstep when a defeated tip is rolled back.
    ///
    /// Under the guard: the delta is removed from the applied map, its
    /// nullifiers are unmarked (the registry's SOLE scoped removal exception
    /// — only the exact keys of the defeated block's delta, under this same
    /// guard, so no concurrent spender's key can be touched), and the tree /
    /// leaf sequence / root history are rebuilt from the surviving deltas.
    /// Rebuilding (rather than an in-place undo) is what makes a mid-history
    /// removal correct: every surviving delta after the removal point gets
    /// its derived `post_root` recomputed against the new leaf prefix.
    ///
    /// `PoolRollback` when the block has no recorded delta (never a silent
    /// skip); a post-rebuild integrity re-assertion (root-history tip ==
    /// tree root) fails closed.
    pub fn revert_update(&self, block_hash: &[u8]) -> Result<(), CommitterError> {
        let mut state = self.guard.lock().unwrap_or_else(|p| p.into_inner());
        let idx = *state
            .applied_index
            .get(block_hash)
            .ok_or_else(|| CommitterError::PoolRollback {
                block: bytes_to_hex(block_hash),
            })?;
        let removed = state.applied.remove(idx);
        // The scoped exception (NullifierRegistry docs): unmark exactly the
        // defeated delta's keys, under this guard.
        self.nullifiers.unmark_many(&removed.nullifiers);
        // Rebuild from the survivors (recomputing derived post_roots).
        let rebuilt = rebuild_state(&state.applied, self.recency_window, /*verify_recorded=*/ false)?;
        // Fail-closed integrity re-assertion (S4.3): the history tip must
        // equal the tree root.
        if rebuilt.roots.current_root() != root_to_bytes(&rebuilt.tree.root()) {
            return Err(CommitterError::PoolState {
                kind: "integrity",
                cause: "post-rollback: root-history tip != tree root".to_string(),
            });
        }
        state.leaves = rebuilt.leaves;
        state.tree = rebuilt.tree;
        state.roots = rebuilt.roots;
        state.applied = rebuilt.applied;
        state.applied_index = rebuilt.applied_index;
        Ok(())
    }

    /// Persist the current pool state (the roadmap 2.5 durable record).
    /// Called by the commit path AFTER every applied delta and BEFORE the
    /// block is reported committed, and after every rollback.
    pub fn save(&self, provider: &dyn DataProvider, partition_id: &str) -> Result<(), CommitterError> {
        let state = self.state_snapshot();
        provider
            .save_shielded_pool(&state, partition_id)
            .map_err(|e| CommitterError::PoolPersist {
                cause: format!("save_shielded_pool({partition_id}) failed: {e:?}"),
            })
    }

    /// The current pool state as its persistable form (nullifiers = the union
    /// of the applied deltas' nullifiers, per the type doc).
    pub fn state_snapshot(&self) -> ShieldedPoolState {
        let state = self.guard.lock().unwrap_or_else(|p| p.into_inner());
        let nullifiers: Vec<[u8; 32]> = state
            .applied
            .iter()
            .flat_map(|d| d.nullifiers.iter().copied())
            .collect();
        ShieldedPoolState {
            root: root_to_bytes(&state.tree.root()),
            leaf_count: state.leaves.len() as u64,
            leaves: state.leaves.iter().map(|l| root_to_bytes(l)).collect(),
            nullifiers,
            applied: state.applied.clone(),
        }
    }

    /// The view's two read sides, at the guard's CURRENT published state:
    /// the shared nullifier registry (live) and the root state as of now
    /// (a snapshot `Arc` — the live value, clowned). The composite build
    /// composes the sentinel's/finalizer's `SimpleShieldedPoolView` from this
    /// at boot (the S5.4 swap site).
    pub fn view_parts(&self) -> (Arc<NullifierRegistry>, Arc<MerkleRootState>) {
        let state = self.guard.lock().unwrap_or_else(|p| p.into_inner());
        (Arc::clone(&self.nullifiers), Arc::clone(&state.roots))
    }

    /// The recency window (consensus K) this pool was built with.
    pub fn recency_window(&self) -> usize {
        self.recency_window
    }

    /// The current pool root (the guard's tree root).
    /// The shared spent-nullifier registry (S4.2) — the same `Arc` the
    /// shielded roles' validation views read.
    pub fn nullifiers(&self) -> &NullifierRegistry {
        &self.nullifiers
    }

    pub fn current_root(&self) -> [u8; 32] {
        let state = self.guard.lock().unwrap_or_else(|p| p.into_inner());
        root_to_bytes(&state.tree.root())
    }

    /// The current leaf count.
    pub fn leaf_count(&self) -> u64 {
        let state = self.guard.lock().unwrap_or_else(|p| p.into_inner());
        state.leaves.len() as u64
    }

    /// The number of applied deltas in the history.
    pub fn applied_count(&self) -> usize {
        let state = self.guard.lock().unwrap_or_else(|p| p.into_inner());
        state.applied.len()
    }
}

/// S5.1's read-only seam. See the module docs ("The view's `root_history`
/// is a boot-time snapshot") for the `root_history` contract and why S5.4
/// owns the live role-view design.
impl ShieldedPoolView for ShieldedPool {
    fn nullifier_set(&self) -> &dyn NullifierMembership {
        &*self.nullifiers
    }

    fn root_history(&self) -> &dyn MerkleRootHistory {
        &*self.view_roots
    }
}

/// Rebuild the guarded state from an ordered delta sequence: re-append every
/// leaf, push every (re)computed post root, re-derive the applied index.
///
/// With `verify_recorded` (the boot path) each delta's recorded `post_root`
/// must equal the recomputed value — a recorded history inconsistent with the
/// leaf sequence is `PoolState { kind: "integrity" }`. Without it (the
/// rollback path, after a mid-history removal) the recomputed values are
/// authoritative and written back: the `post_root` is a *derived* value and
/// the next `save` re-fingerprints the whole state.
fn rebuild_state(
    deltas: &[AppliedPoolDelta],
    recency_window: usize,
    verify_recorded: bool,
) -> Result<PoolState, CommitterError> {
    let mut leaves: Vec<Fp> = Vec::new();
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let mut roots = MerkleRootState::new(recency_window);
    let mut applied: Vec<AppliedPoolDelta> = Vec::with_capacity(deltas.len());
    let mut applied_index: HashMap<Vec<u8>, usize> = HashMap::new();

    for delta in deltas {
        for leaf in &delta.leaves {
            let f = bytes_to_root(leaf).ok_or_else(|| CommitterError::PoolState {
                kind: "invalid_root",
                cause: format!(
                    "delta for block {} carries a leaf that is not a valid Fp",
                    bytes_to_hex(&delta.block_hash)
                ),
            })?;
            tree.append_leaf(&f);
            leaves.push(f);
        }
        let post_root = root_to_bytes(&tree.root());
        if verify_recorded && post_root != delta.post_root {
            return Err(CommitterError::PoolState {
                kind: "integrity",
                cause: format!(
                    "delta for block {}'s recorded post_root is inconsistent with the leaf \
                     sequence",
                    bytes_to_hex(&delta.block_hash)
                ),
            });
        }
        roots.push(post_root);
        applied_index.insert(delta.block_hash.clone(), applied.len());
        applied.push(AppliedPoolDelta {
            block_hash: delta.block_hash.clone(),
            leaves: delta.leaves.clone(),
            nullifiers: delta.nullifiers.clone(),
            post_root,
        });
    }
    Ok(PoolState {
        leaves,
        tree,
        roots: Arc::new(roots),
        applied,
        applied_index,
    })
}

#[cfg(test)]
impl ShieldedPool {
    /// Test hook: record a synthetic applied delta (a prior commit's
    /// preimage) WITHOUT the consensus re-check, so the live pipeline tests
    /// can seed the pool state a prior block would have produced (the
    /// sentinel-fixture pattern: a mirror tree, a membership proof, a real
    /// transfer against it). Sole caller: the test suite.
    pub(crate) fn record_delta_for_tests(
        &self,
        delta: &AppliedPoolDelta,
    ) -> Result<(), CommitterError> {
        let mut state = self.guard.lock().unwrap_or_else(|p| p.into_inner());
        if state.applied_index.contains_key(&delta.block_hash) {
            return Ok(()); // already recorded — idempotent
        }
        for leaf in &delta.leaves {
            let f = bytes_to_root(leaf).ok_or_else(|| CommitterError::PoolState {
                kind: "invalid_root",
                cause: format!(
                    "test delta for block {} carries a leaf that is not a valid Fp",
                    bytes_to_hex(&delta.block_hash)
                ),
            })?;
            state.tree.append_leaf(&f);
            state.leaves.push(f);
        }
        self.nullifiers
            .mark_many_atomic(&delta.nullifiers)
            .map_err(|e| CommitterError::PoolState {
                kind: "integrity",
                cause: format!("test delta double-spends a nullifier: {e:?}"),
            })?;
        let post_root = root_to_bytes(&state.tree.root());
        let mut staged = (*state.roots).clone();
        staged.push(post_root);
        state.roots = Arc::new(staged);
        let delta_index = state.applied.len();
        state.applied_index.insert(delta.block_hash.clone(), delta_index);
        state.applied.push(delta.clone());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pneumatic_core::blocks::FinalityStatus;
    use pneumatic_core::transactions::{SignedTransaction, Transaction, TransactionSignature};

    /// A distinct dummy 32-byte value from a tag byte.
    fn dummy(tag: u8) -> [u8; 32] {
        let mut v = [0u8; 32];
        v[0] = tag;
        v
    }

    /// A dummy block whose current_hash is `tag`-derived and which carries the
    /// given shielded payload.
    fn block_with_hash(tag: u8, shielded: Option<pneumatic_core::transactions::ShieldedTransaction>) -> Block {
        let mut b: Block = serde_test_block(shielded);
        b.current_hash = dummy(tag).to_vec();
        b
    }

    /// A minimal wire block (plain by default) with the fields the pool path
    /// reads.
    fn serde_test_block(shielded: Option<pneumatic_core::transactions::ShieldedTransaction>) -> Block {
        Block {
            signed_trans: SignedTransaction {
                shielded,
                transaction_id: "tx".to_string(),
                transaction: Transaction {
                    id: "tx".to_string(),
                    action: "ShieldedTransfer".into(),
                    token_id: vec![1],
                    bid: None,
                    sequence_number: 1,
                    sender: b"alice".to_vec(),
                    receiver: b"bob".to_vec(),
                    amount: None,
                    timestamp: 0,
                    result_hash: vec![],
                    sender_signature: vec![],
                },
                total_voters: 3,
                total_stake: 42,
                leader_address: vec![],
                leader_stake: 0,
                leader_hash: vec![1, 2, 3],
                finalizer_addr: vec![],
                finalizer_sig: TransactionSignature {
                    transaction_id: vec![],
                    env_id: vec![],
                    transaction_hash: vec![],
                    signature: vec![],
                    current_stake: 0,
                },
                executor_sigs: HashMap::new(),
                proposer_key: vec![],
            },
            token_metadata: HashMap::new(),
            previous_hash: vec![1, 2, 3],
            current_hash: vec![],
            timestamp: 0,
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        }
    }

    fn make_env() -> EnvironmentMetadata {
        let json = r#"{"environment_id":"test","environment_name":"test",
            "partitions":[{"id":"token","partition_type":"Token"},
            {"id":"slush","partition_type":"Slush"}],
            "asym_crypto_provider":{"Ed25519":null},"sym_crypto_provider":"sym",
            "serialization_provider":"rmp","quorum_percentage":67.0,
            "override_quorum_percentage":0.0,"max_risk":1.0,
            "allowed_token_types":[],"trans_validation_specs":[],
            "block_validation_specs":[],"log_file":"test.log"}"#;
        let spec: pneumatic_core::environment::EnvironmentMetadataSpec =
            serde_json::from_str(json).expect("spec JSON");
        EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec")
    }

    /// A structurally-valid shielded tx (S4.1 check 1 passes: 1 nullifier, 1
    /// spent commitment, 1 output commitment; nullifiers distinct) whose
    /// commitments decode to the on-curve point (1,0) — the 32-byte
    /// little-endian encoding of x=1, y=0 (y² = x³ − x ⇒ (1,0) is on the
    /// Pallas curve) — so checks 1-3 run and a garbage proof (check 4) is the
    /// variable under test. `merkle_root` is set so check 3 is the variable.
    fn fake_stx(id: &str, nullifier: [u8; 32], merkle_root: [u8; 32]) -> pneumatic_core::transactions::ShieldedTransaction {
        pneumatic_core::transactions::ShieldedTransaction {
            id: id.to_string(),
            action: "ShieldedTransfer".to_string(),
            token_id: vec![1],
            spent_commitments: vec![dummy(1)],
            nullifiers: vec![nullifier],
            commitments: vec![dummy(1)],
            merkle_root,
            proof: vec![0u8; 64],
            note_ciphertexts: vec![vec![0u8; 16]],
            fee: 10,
        }
    }

    /// The in-memory `DataProvider` for the boot tests: stores one
    /// envelope-verified pool state, with a get-failure toggle.
    struct PoolTestProvider {
        pool_state: std::sync::Mutex<Option<pneumatic_core::data::ShieldedPoolStateEnvelope>>,
        fail_get: bool,
        fail_save: bool,
        saved: std::sync::Mutex<Vec<pneumatic_core::data::ShieldedPoolState>>,
    }

    impl PoolTestProvider {
        fn new() -> Self {
            Self {
                pool_state: std::sync::Mutex::new(None),
                fail_get: false,
                fail_save: false,
                saved: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn with_state(mut self, state: pneumatic_core::data::ShieldedPoolState, corrupt: bool) -> Self {
            let mut env = pneumatic_core::data::ShieldedPoolStateEnvelope::new(state);
            if corrupt {
                env.hash = [0u8; 32];
            }
            *self.pool_state.lock().unwrap() = Some(env);
            self
        }
        fn with_fail_get(mut self, fail: bool) -> Self {
            self.fail_get = fail;
            self
        }
        fn with_fail_save(mut self, fail: bool) -> Self {
            self.fail_save = fail;
            self
        }
        fn stored(&self) -> Option<pneumatic_core::data::ShieldedPoolState> {
            self.pool_state
                .lock()
                .unwrap()
                .as_ref()
                .map(|e| e.payload.clone())
        }
        fn saves(&self) -> Vec<pneumatic_core::data::ShieldedPoolState> {
            self.saved.lock().unwrap().clone()
        }
    }

    impl DataProvider for PoolTestProvider {
        fn get_shielded_pool(
            &self,
            _partition_id: &str,
        ) -> Result<Option<pneumatic_core::data::ShieldedPoolState>, pneumatic_core::data::DataError> {
            if self.fail_get {
                return Err(pneumatic_core::data::DataError::FromStore(
                    "simulated data-service failure".into(),
                ));
            }
            let env = self.pool_state.lock().unwrap().clone();
            match env {
                Some(env) => {
                    env.verify()?;
                    Ok(Some(env.payload))
                }
                None => Ok(None),
            }
        }
        fn save_shielded_pool(
            &self,
            state: &pneumatic_core::data::ShieldedPoolState,
            _partition_id: &str,
        ) -> Result<(), pneumatic_core::data::DataError> {
            if self.fail_save {
                return Err(pneumatic_core::data::DataError::FromStore(
                    "simulated save failure".into(),
                ));
            }
            let env = pneumatic_core::data::ShieldedPoolStateEnvelope::new(state.clone());
            *self.pool_state.lock().unwrap() = Some(env);
            self.saved.lock().unwrap().push(state.clone());
            Ok(())
        }
        // The pool tests never touch stake/executor snapshots — reject loudly.
        fn get_stake_snapshot(
            &self,
            _epoch: u64,
            _partition_id: &str,
        ) -> Result<pneumatic_core::epoch::StakeSet, pneumatic_core::data::DataError> {
            Err(pneumatic_core::data::DataError::CacheError)
        }
        fn save_stake_snapshot(
            &self,
            _epoch: u64,
            _snapshot: pneumatic_core::epoch::StakeSet,
            _partition_id: &str,
        ) -> Result<(), pneumatic_core::data::DataError> {
            Err(pneumatic_core::data::DataError::CacheError)
        }
        fn get_executor_set(
            &self,
            _epoch: u64,
            _partition_id: &str,
        ) -> Result<pneumatic_core::epoch::ExecutorSet, pneumatic_core::data::DataError> {
            Err(pneumatic_core::data::DataError::CacheError)
        }
        fn save_executor_set(
            &self,
            _epoch: u64,
            _set: pneumatic_core::epoch::ExecutorSet,
            _partition_id: &str,
        ) -> Result<(), pneumatic_core::data::DataError> {
            Err(pneumatic_core::data::DataError::CacheError)
        }
    }

    /// A pristine pool state (genesis: zero root, no leaves, no deltas).
    fn pristine_state() -> pneumatic_core::data::ShieldedPoolState {
        pneumatic_core::data::ShieldedPoolState {
            root: [0u8; 32],
            leaf_count: 0,
            leaves: vec![],
            nullifiers: vec![],
            applied: vec![],
        }
    }

    /// The Fp encoding of a dummy leaf (the pool's leaves are Fp; the dummy
    /// byte values here are valid encodings iff they decode as an Fp —
    /// `Fp::from_canonical_bytes` on a small value is a valid field element).
    fn dummy_leaf(tag: u8) -> [u8; 32] {
        let mut v = [0u8; 32];
        v[0] = tag;
        v
    }

    #[test]
    fn new_pool_is_genesis_pristine() {
        let pool = ShieldedPool::new(10);
        assert_eq!(pool.current_root(), [0u8; 32]);
        assert_eq!(pool.leaf_count(), 0);
        assert_eq!(pool.applied_count(), 0);
        assert_eq!(pool.recency_window(), 10);
    }

    #[test]
    fn load_seeds_and_pristine_when_absent() {
        // Ok(None) — a true absence — seeds a pristine genesis AND persists it.
        let provider = PoolTestProvider::new();
        let pool = ShieldedPool::load(&provider, "token", 10).expect("fresh load succeeds");
        assert_eq!(pool.current_root(), [0u8; 32]);
        assert_eq!(pool.leaf_count(), 0);
        assert_eq!(provider.stored(), Some(pristine_state()), "the pristine seed is persisted");
    }

    #[test]
    fn load_fails_closed_on_provider_error() {
        // Err from get_shielded_pool is NEVER treated as "no state".
        let provider = PoolTestProvider::new().with_fail_get(true);
        let err = ShieldedPool::load(&provider, "token", 10).expect_err("provider error must fail boot");
        match err {
            CommitterError::PoolState { kind, .. } => assert_eq!(kind, "corrupt"),
            other => panic!("expected PoolState kind=corrupt, got {other:?}"),
        }
    }

    #[test]
    fn load_fails_closed_on_corrupted_envelope() {
        // A stored envelope whose fingerprint does not match its payload.
        let provider = PoolTestProvider::new().with_state(pristine_state(), true);
        let err = ShieldedPool::load(&provider, "token", 10).expect_err("corrupt envelope must fail boot");
        match err {
            CommitterError::PoolState { kind, .. } => assert_eq!(kind, "corrupt"),
            other => panic!("expected PoolState kind=corrupt, got {other:?}"),
        }
    }

    #[test]
    fn load_fails_closed_on_leaf_count_mismatch() {
        // The payload's envelope is valid, but leaf_count contradicts leaves.
        let mut state = pristine_state();
        state.leaf_count = 3;
        let provider = PoolTestProvider::new().with_state(state, false);
        let err = ShieldedPool::load(&provider, "token", 10).expect_err("integrity fault must fail boot");
        match err {
            CommitterError::PoolState { kind, .. } => assert_eq!(kind, "integrity"),
            other => panic!("expected PoolState kind=integrity, got {other:?}"),
        }
    }

    #[test]
    fn load_rebuilds_tree_and_history_from_deltas() {
        // Two deltas with real Fp leaves: boot must rebuild the exact tree,
        // root history, nullifier set, and applied map.
        let seed = ShieldedPool::new(10);
        let leaf_a = Fp::from(7u64);
        let leaf_b = Fp::from(9u64);
        let root_a = {
            // Build a real tree so the post_roots are genuine.
            let mut t = IncrementalMerkleTree::new(DEFAULT_DEPTH);
            t.append_leaf(&leaf_a);
            root_to_bytes(&t.root())
        };
        let root_b = {
            let mut t = IncrementalMerkleTree::new(DEFAULT_DEPTH);
            t.append_leaf(&leaf_a);
            t.append_leaf(&leaf_b);
            root_to_bytes(&t.root())
        };
        let hash_a = vec![0xAAu8; 32];
        let hash_b = vec![0xBBu8; 32];
        let null_a = dummy(0xA1);
        let null_b = dummy(0xB1);
        let delta_a = AppliedPoolDelta {
            block_hash: hash_a.clone(),
            leaves: vec![root_to_bytes(&leaf_a)],
            nullifiers: vec![null_a],
            post_root: root_a,
        };
        let delta_b = AppliedPoolDelta {
            block_hash: hash_b.clone(),
            leaves: vec![root_to_bytes(&leaf_b)],
            nullifiers: vec![null_b],
            post_root: root_b,
        };
        seed.record_delta_for_tests(&delta_a).expect("seed a");
        seed.record_delta_for_tests(&delta_b).expect("seed b");

        let persisted = seed.state_snapshot();
        assert_eq!(persisted.root, root_b);
        assert_eq!(persisted.leaf_count, 2);

        // Boot from the persisted state.
        let provider = PoolTestProvider::new().with_state(persisted.clone(), false);
        let loaded = ShieldedPool::load(&provider, "token", 10).expect("consistent state loads");
        assert_eq!(loaded.current_root(), root_b, "tree rebuilt to the same root");
        assert_eq!(loaded.leaf_count(), 2);
        assert_eq!(loaded.applied_count(), 2);
        assert_eq!(loaded.state_snapshot(), persisted, "round-trip is exact");
        // The nullifier set is rebuilt (check-2 data for future transfers).
        assert!(
            NullifierMembership::contains_nullifier(&*loaded.view_parts().0, null_a)
                && NullifierMembership::contains_nullifier(&*loaded.view_parts().0, null_b)
        );
    }

    #[test]
    fn load_fails_closed_when_post_root_history_diverges() {
        // A recorded post_root the leaf sequence does not reproduce — the
        // "recorded post_root history inconsistent with the leaf sequence"
        // kind from the S5.3 plan.
        let seed = ShieldedPool::new(10);
        let leaf_a = Fp::from(7u64);
        let true_root = {
            let mut t = IncrementalMerkleTree::new(DEFAULT_DEPTH);
            t.append_leaf(&leaf_a);
            root_to_bytes(&t.root())
        };
        let mut state = seed.state_snapshot_after(dummy_delta(true_root));
        // Corrupt: claim a post_root the leaves do not produce.
        state.applied[0].post_root = dummy(0xEE);
        state.root = dummy(0xEE);
        let provider = PoolTestProvider::new().with_state(state, false);
        let err = ShieldedPool::load(&provider, "token", 10).expect_err("divergent history must fail boot");
        match err {
            CommitterError::PoolState { kind, .. } => assert_eq!(kind, "integrity"),
            other => panic!("expected PoolState kind=integrity, got {other:?}"),
        }
    }

    fn dummy_delta(post_root: [u8; 32]) -> AppliedPoolDelta {
        AppliedPoolDelta {
            block_hash: vec![0xAA; 32],
            leaves: vec![],
            nullifiers: vec![],
            post_root,
        }
    }

    // A one-shot state snapshot helper (the delta's own view) — avoids
    // reaching into the pool's private state from this test module.
    trait SnapshotExt {
        fn state_snapshot_after(&self, delta: AppliedPoolDelta) -> pneumatic_core::data::ShieldedPoolState;
    }
    impl SnapshotExt for ShieldedPool {
        fn state_snapshot_after(&self, delta: AppliedPoolDelta) -> pneumatic_core::data::ShieldedPoolState {
            // Record the (corrupt) delta's claimed values into a fresh
            // pristine state — the corruption is in the delta's post_root,
            // which the rebuild recomputes and must reject.
            let mut state = pristine_state();
            state.root = delta.post_root;
            state.applied = vec![delta];
            state
        }
    }

    #[test]
    fn apply_update_rejects_bad_proof_with_pool_owned_deps() {
        // Discriminator 5's fast form: a structurally-valid tx (checks 1-3
        // pass: fresh nullifier, genesis root is fresh) with a garbage proof
        // is rejected by the POOL's own re-check — the committer's verdict,
        // not the sentinel's/finalizer's. The pool stays untouched.
        let pool = ShieldedPool::new(10);
        let env = make_env();
        let stx = fake_stx("tx1", dummy(0x55), [0u8; 32]); // genesis root → check 3 passes
        let block = block_with_hash(1, Some(stx));
        let err = pool
            .apply_update(&block, &env)
            .expect_err("garbage proof must fail the re-check");
        match err {
            CommitterError::ShieldedProofInvalid { tx_id, cause } => {
                assert_eq!(tx_id, "tx1");
                assert!(cause.contains("InvalidShieldedProof"), "cause: {cause}");
            }
            other => panic!("expected ShieldedProofInvalid, got {other:?}"),
        }
        assert_eq!(pool.leaf_count(), 0, "the pool is untouched");
        assert_eq!(pool.applied_count(), 0);
    }

    #[test]
    fn apply_update_rejects_stale_nullifier() {
        // Check 2: a nullifier the pool already spent is StaleNullifier —
        // the double-spend guard, on the pool's own set.
        let pool = ShieldedPool::new(10);
        let env = make_env();
        let spent = dummy(0x55);
        pool.nullifiers
            .mark_many_atomic(&[spent])
            .expect("mark the nullifier");
        let stx = fake_stx("tx1", spent, [0u8; 32]);
        let block = block_with_hash(1, Some(stx));
        let err = pool.apply_update(&block, &env).expect_err("spent nullifier must be rejected");
        match err {
            CommitterError::ShieldedProofInvalid { cause, .. } => {
                assert!(cause.contains("StaleNullifier"), "cause: {cause}");
            }
            other => panic!("expected ShieldedProofInvalid, got {other:?}"),
        }
    }

    #[test]
    fn apply_update_rejects_stale_root() {
        // Check 3: a referenced root that is not in the pool's history is
        // StaleMerkleRoot (the recency guard).
        let pool = ShieldedPool::new(10);
        let env = make_env();
        let stx = fake_stx("tx1", dummy(0x55), dummy(0x99)); // unknown root
        let block = block_with_hash(1, Some(stx));
        let err = pool.apply_update(&block, &env).expect_err("unknown root must be rejected");
        match err {
            CommitterError::ShieldedProofInvalid { cause, .. } => {
                assert!(cause.contains("StaleMerkleRoot"), "cause: {cause}");
            }
            other => panic!("expected ShieldedProofInvalid, got {other:?}"),
        }
    }

    #[test]
    fn apply_update_replay_is_already_applied_before_recheck() {
        // Idempotency discriminator (fast form): a block whose hash is ALREADY
        // in the applied index returns `AlreadyApplied` immediately — the
        // consensus re-check does NOT re-run. The replayed stx deliberately
        // references the already-spent nullifier (plus a garbage proof): if
        // re-validation ran, it would reject with `StaleNullifier`; the
        // idempotency check wins. No double-append, no double-mark.
        let pool = ShieldedPool::new(10);
        let env = make_env();
        let leaf = root_to_bytes(&Fp::from(7u64));
        let nullifier = dummy(0x55);
        let delta = pneumatic_core::data::AppliedPoolDelta {
            block_hash: dummy(0x77).to_vec(),
            leaves: vec![leaf],
            nullifiers: vec![nullifier],
            post_root: [0u8; 32],
        };
        pool.record_delta_for_tests(&delta).expect("seed the delta");
        assert!(pool.nullifiers.contains(nullifier), "nullifier is marked");

        // Replayed block: same hash, stale-nullifier stx, garbage proof.
        let stx = fake_stx("tx1", nullifier, [0u8; 32]);
        let block = block_with_hash(0x77, Some(stx));
        assert_eq!(
            pool.apply_update(&block, &env).expect("replay must not fail"),
            PoolApplyOutcome::AlreadyApplied,
            "a recorded hash short-circuits before re-validation"
        );
        assert_eq!(pool.leaf_count(), 1, "no double-append on replay");
        assert_eq!(pool.applied_count(), 1, "no double-record on replay");
    }

    #[test]
    fn save_fails_with_pool_persist_on_store_failure() {
        // Durability seam: a data-service save failure surfaces as
        // `PoolPersist` (the commit path treats this as fail-closed — the
        // chain append is undone, never a silently-unpersisted delta).
        let dp = Arc::new(PoolTestProvider::new().with_fail_save(true));
        let pool = ShieldedPool::new(10);
        let err = pool
            .save(&*dp, "token")
            .expect_err("save must fail on store failure");
        assert!(matches!(err, CommitterError::PoolPersist { .. }), "got {err:?}");
    }

    #[test]
    fn apply_update_is_plain_noop_for_unshielded_blocks() {
        let pool = ShieldedPool::new(10);
        let env = make_env();
        let block = block_with_hash(1, None);
        assert_eq!(
            pool.apply_update(&block, &env).expect("plain block applies"),
            PoolApplyOutcome::Plain
        );
        assert_eq!(pool.leaf_count(), 0);
        assert_eq!(pool.applied_count(), 0);
    }

    #[test]
    fn revert_update_is_the_exact_inverse_of_recorded_delta() {
        // Mechanism (the fast form of discriminator 7): a recorded delta is
        // fully undone — tree, leaf sequence, root history, nullifiers, and
        // the idempotency index all return to their prior state; the block
        // hash leaves the applied map (so a re-applied block re-records).
        let pool = ShieldedPool::new(10);
        let leaf_a = Fp::from(7u64);
        let leaf_b = Fp::from(9u64);
        let root_a = {
            let mut t = IncrementalMerkleTree::new(DEFAULT_DEPTH);
            t.append_leaf(&leaf_a);
            root_to_bytes(&t.root())
        };
        let root_b = {
            let mut t = IncrementalMerkleTree::new(DEFAULT_DEPTH);
            t.append_leaf(&leaf_a);
            t.append_leaf(&leaf_b);
            root_to_bytes(&t.root())
        };
        let hash_a = vec![0xAAu8; 32];
        let hash_b = vec![0xBBu8; 32];
        let null_a = dummy(0xA1);
        pool.record_delta_for_tests(&AppliedPoolDelta {
            block_hash: hash_a.clone(),
            leaves: vec![root_to_bytes(&leaf_a)],
            nullifiers: vec![null_a],
            post_root: root_a,
        })
        .expect("seed a");
        pool.record_delta_for_tests(&AppliedPoolDelta {
            block_hash: hash_b.clone(),
            leaves: vec![root_to_bytes(&leaf_b)],
            nullifiers: vec![],
            post_root: root_b,
        })
        .expect("seed b");
        assert_eq!(pool.current_root(), root_b);

        // Revert the FIRST delta (mid-history removal): the second's derived
        // post_root must be recomputed against the new prefix.
        pool.revert_update(&hash_a).expect("revert a");
        let mut t = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        t.append_leaf(&leaf_b);
        let expected_root = root_to_bytes(&t.root());
        assert_eq!(pool.current_root(), expected_root, "rebuilt over the surviving leaves");
        assert_eq!(pool.leaf_count(), 1);
        assert_eq!(pool.applied_count(), 1);
        assert!(!NullifierMembership::contains_nullifier(&*pool.view_parts().0, null_a), "nullifier unmarked");
        // The loser is no longer in the idempotency map: re-recording it is
        // clean (the chain re-appending it re-applies cleanly).
        pool.record_delta_for_tests(&AppliedPoolDelta {
            block_hash: hash_a.clone(),
            leaves: vec![root_to_bytes(&leaf_a)],
            nullifiers: vec![null_a],
            post_root: expected_root,
        })
        .expect("re-record after revert");
        assert_eq!(pool.applied_count(), 2);
    }

    #[test]
    fn revert_unknown_block_is_pool_rollback_not_silent_skip() {
        let pool = ShieldedPool::new(10);
        let err = pool.revert_update(&[0u8; 32]).expect_err("no delta recorded");
        assert!(matches!(err, CommitterError::PoolRollback { .. }));
    }

    #[test]
    fn save_persists_snapshot_and_load_restores_it() {
        // The durability round-trip (discriminator 1's persistence half,
        // without a live proof): apply a recorded delta, save, load, exact
        // equality.
        let provider = PoolTestProvider::new();
        let pool = ShieldedPool::load(&provider, "token", 10).expect("boot");
        let leaf = Fp::from(42u64);
        let mut t = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        t.append_leaf(&leaf);
        let root = root_to_bytes(&t.root());
        let hash = vec![0xCCu8; 32];
        pool.record_delta_for_tests(&AppliedPoolDelta {
            block_hash: hash,
            leaves: vec![root_to_bytes(&leaf)],
            nullifiers: vec![dummy(0xC1)],
            post_root: root,
        })
        .expect("seed");
        pool.save(&provider, "token").expect("persist");
        assert_eq!(provider.saves().len(), 2, "boot seed + the delta");
        let reloaded = ShieldedPool::load(&provider, "token", 10).expect("reload");
        assert_eq!(reloaded.state_snapshot(), pool.state_snapshot());
        assert_eq!(reloaded.current_root(), root);
    }

    #[test]
    fn view_contract_nullifiers_live_and_roots_boot_snapshot() {
        // The trait impl: nullifier_set is the shared registry (live);
        // root_history is the stable boot-time snapshot (module docs).
        let pool = ShieldedPool::new(10);
        let nf = dummy(0x77);
        pool.nullifiers.mark_many_atomic(&[nf]).expect("mark");
        assert!(NullifierMembership::contains_nullifier(pool.nullifier_set(), nf));
        assert_eq!(pool.root_history().root_history().len(), 1, "boot snapshot = genesis only");
        // Send + Sync (the seam's pinned contract).
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ShieldedPool>();
    }

    #[test]
    fn view_parts_returns_live_published_state() {
        // view_parts is the live value (a snapshot Arc clone at call time) —
        // the composite build composes the roles' simple views from it.
        let pool = ShieldedPool::new(10);
        let leaf = Fp::from(5u64);
        let mut t = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        t.append_leaf(&leaf);
        let root = root_to_bytes(&t.root());
        pool.record_delta_for_tests(&AppliedPoolDelta {
            block_hash: vec![0xDD; 32],
            leaves: vec![root_to_bytes(&leaf)],
            nullifiers: vec![],
            post_root: root,
        })
        .expect("seed");
        let (_nullifiers, roots) = pool.view_parts();
        assert_eq!(roots.current_root(), root, "live published root");
        assert_eq!(roots.len(), 2, "genesis + the delta");
    }
}
