//! Phase S1.6 — Incremental Merkle tree over note commitments.
//!
//! Append-only binary Merkle tree with fixed depth (default 32, parameterized).
//! Leaves are `Fp` field elements: the Poseidon hash of a commitment's affine
//! `(x, y)` coordinates. Internal nodes are `poseidon_hash([left, right])`.
//!
//! The tree operates entirely in `Fp` (the Pallas base field), matching
//! Poseidon's input domain. Commitments are `EpAffine` curve points (S1.3);
//! the bridge is `leaf = poseidon_hash([commit.x, commit.y])`.
//!
//! # Operations
//!
//! - `append(commitment) -> (new_root, MembershipProof)` — O(log n)
//! - `verify_proof(leaf, proof, root, depth) -> bool` — O(log n)
//! - `root() -> Fp` — current root (`Fp::zero()` for empty tree)
//! - `snapshot(since) -> TreeState` — for S5 committer persistence
//! - `root_to_bytes(root) -> [u8; 32]` — canonical wire encoding
//!
//! # Serde
//!
//! `Fp` has no `Serialize`/`Deserialize` impl. `MembershipProof` and
//! `TreeState` use manual serde: each `Fp` ↔ 32 bytes via `to_repr()` /
//! `from_repr()`. Deserialization fails closed on invalid field elements.

use ff::{Field, PrimeField};
use pasta_curves::arithmetic::CurveAffine;
use pasta_curves::pallas::{Affine as EpAffine, Base as Fp};
use serde::de::{Error as DeError, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::shielded::poseidon::poseidon_hash;

/// Default tree depth: 2^32 = ~4.3 billion leaves.
pub const DEFAULT_DEPTH: u32 = 32;

// ── Fp serde helpers ──────────────────────────────────────────────────────────

/// Convert a `CtOption<Fp>` to `Option<Fp>`, failing closed on `None`.
fn ct_option_to_opt(opt: subtle::CtOption<Fp>) -> Option<Fp> {
    opt.into_option()
}

/// Deserialize a single `Fp` from 32 bytes. Fails closed on invalid field element.
fn fp_from_bytes<'de, D: Deserializer<'de>>(de: D) -> Result<Fp, D::Error> {
    let bytes: [u8; 32] = Deserialize::deserialize(de)?;
    ct_option_to_opt(Fp::from_repr(bytes))
        .ok_or_else(|| D::Error::custom("invalid Fp field element (>= p or bad repr)"))
}

/// Visitor for a sequence of `Fp` elements, each serialized as 32 bytes.
struct FpSeqVisitor;

impl<'de> Visitor<'de> for FpSeqVisitor {
    type Value = Vec<Fp>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a sequence of 32-byte Fp field elements")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<Fp>, A::Error> {
        let mut out = Vec::new();
        while let Some(bytes) = seq.next_element::<[u8; 32]>()? {
            let fp = ct_option_to_opt(Fp::from_repr(bytes))
                .ok_or_else(|| A::Error::custom("invalid Fp field element"))?;
            out.push(fp);
        }
        Ok(out)
    }
}

/// Seed for deserializing a `Vec<Fp>` within a `SeqAccess` context.
struct FpVecSeed;

impl<'de> serde::de::DeserializeSeed<'de> for FpVecSeed {
    type Value = Vec<Fp>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Vec<Fp>, D::Error> {
        deserializer.deserialize_seq(FpSeqVisitor)
    }
}

// ── Core types ────────────────────────────────────────────────────────────────

/// A membership proof: the index of the leaf plus one sibling per level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipProof {
    /// Global leaf index (0-based).
    pub index: u64,
    /// One sibling `Fp` per level, level 0 (leaf level) through level depth-1.
    pub siblings: Vec<Fp>,
}

impl Serialize for MembershipProof {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut seq = ser.serialize_seq(Some(2))?;
        seq.serialize_element(&self.index)?;
        // Serialize siblings as Vec<[u8; 32]> to avoid requiring Fp: Serialize.
        let sibling_bytes: Vec<[u8; 32]> = self.siblings.iter().map(|fp| fp.to_repr()).collect();
        seq.serialize_element(&sibling_bytes)?;
        seq.end()
    }
}

impl<'de> Deserialize<'de> for MembershipProof {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct ProofVisitor;
        impl<'de> Visitor<'de> for ProofVisitor {
            type Value = MembershipProof;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("(index, siblings) pair")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<MembershipProof, A::Error> {
                let index: u64 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("missing index"))?;
                let siblings: Vec<Fp> = seq
                    .next_element_seed(FpVecSeed)?
                    .ok_or_else(|| A::Error::custom("missing siblings"))?;
                Ok(MembershipProof { index, siblings })
            }
        }
        de.deserialize_seq(ProofVisitor)
    }
}

/// Snapshot of tree state for S5 committer persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeState {
    /// Current root.
    pub root: Fp,
    /// Total leaves appended so far.
    pub leaf_count: u64,
    /// Leaves appended since the last snapshot (for replay on reload).
    pub recent_leaves: Vec<Fp>,
}

impl Serialize for TreeState {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut seq = ser.serialize_seq(Some(3))?;
        seq.serialize_element(&self.root.to_repr())?;
        seq.serialize_element(&self.leaf_count)?;
        let leaf_bytes: Vec<[u8; 32]> = self.recent_leaves.iter().map(|fp| fp.to_repr()).collect();
        seq.serialize_element(&leaf_bytes)?;
        seq.end()
    }
}

impl<'de> Deserialize<'de> for TreeState {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct StateVisitor;
        impl<'de> Visitor<'de> for StateVisitor {
            type Value = TreeState;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("(root, leaf_count, recent_leaves) triple")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<TreeState, A::Error> {
                let root_bytes: [u8; 32] = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("missing root"))?;
                let root = ct_option_to_opt(Fp::from_repr(root_bytes))
                    .ok_or_else(|| A::Error::custom("invalid root Fp"))?;
                let leaf_count: u64 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("missing leaf_count"))?;
                let recent_leaves: Vec<Fp> = seq
                    .next_element_seed(FpVecSeed)?
                    .ok_or_else(|| A::Error::custom("missing recent_leaves"))?;
                Ok(TreeState { root, leaf_count, recent_leaves })
            }
        }
        de.deserialize_seq(StateVisitor)
    }
}

// ── IncrementalMerkleTree ─────────────────────────────────────────────────────

/// Append-only incremental (binary, fixed-depth) Merkle tree over `Fp` leaves.
///
/// `levels[l]` holds the nodes at height `l` (level 0 = leaves, level `depth`
/// = root). Nodes are stored in index order as they are computed; unfilled
/// slots are treated as `Fp::zero()` during hashing.
#[derive(Debug, Clone)]
pub struct IncrementalMerkleTree {
    depth: u32,
    levels: Vec<Vec<Fp>>,
    leaf_count: u64,
}

impl IncrementalMerkleTree {
    /// Create a new empty tree with the given depth.
    pub fn new(depth: u32) -> Self {
        Self {
            depth,
            levels: vec![Vec::new(); (depth + 1) as usize],
            leaf_count: 0,
        }
    }

    /// Create a new empty tree with [`DEFAULT_DEPTH`] (32).
    pub fn default_depth() -> Self {
        Self::new(DEFAULT_DEPTH)
    }

    /// Restore constructor (S5.3): rebuild a tree over an existing leaf
    /// sequence by re-appending each leaf in order.
    ///
    /// Used by the committer's shielded pool on rollback and boot reload,
    /// where the full ordered leaf list is persisted. The re-append is
    /// O(n·depth) — acceptable at v1 scale (32 B/leaf is already the
    /// audited persistence cost). The result is bit-identical to a tree
    /// built by the same append sequence, so any membership proof that
    /// verified before still verifies against the restored root.
    pub fn from_leaves(depth: u32, leaves: &[Fp]) -> Self {
        let mut tree = Self::new(depth);
        for leaf in leaves {
            tree.append_leaf(leaf);
        }
        tree
    }

    /// The tree's fixed depth.
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Number of leaves appended so far.
    pub fn leaf_count(&self) -> u64 {
        self.leaf_count
    }

    /// Current root. `Fp::zero()` for an empty tree.
    pub fn root(&self) -> Fp {
        if self.leaf_count == 0 {
            Fp::zero()
        } else {
            self.levels[self.depth as usize][0]
        }
    }

    /// Compute the `Fp` leaf for a commitment: `poseidon_hash([x, y])`.
    ///
    /// Panics if the commitment is the point at infinity (which a Pedersen
    /// commitment from S1.3 will never be in practice).
    fn commitment_to_leaf(commitment: &EpAffine) -> Fp {
        let coords = commitment
            .coordinates()
            .expect("commitment must not be the point at infinity");
        poseidon_hash(&[coords.x().clone(), coords.y().clone()])
    }

    /// Append a commitment to the tree.
    ///
    /// Computes the leaf as `poseidon_hash([commit.x, commit.y])`, updates
    /// the affected path to the root in O(log n), and returns the new root
    /// along with a membership proof for the newly-appended leaf.
    pub fn append(&mut self, commitment: &EpAffine) -> (Fp, MembershipProof) {
        let leaf = Self::commitment_to_leaf(commitment);
        self.append_leaf(&leaf)
    }

    /// Append a raw `Fp` leaf, updating the affected path to the root in
    /// O(log n). Returns the new root and the membership proof for the
    /// newly-appended leaf. `append` is the commitment convenience wrapper
    /// over this; `from_leaves` reuses it for restore.
    pub fn append_leaf(&mut self, leaf: &Fp) -> (Fp, MembershipProof) {
        let index = self.leaf_count;

        // Push leaf at level 0.
        self.levels[0].push(*leaf);

        let mut siblings = Vec::with_capacity(self.depth as usize);
        let mut current = *leaf;

        for l in 0..self.depth {
            let pos = index >> l;
            let sib_pos = pos ^ 1;
            let sib = self.levels[l as usize]
                .get(sib_pos as usize)
                .copied()
                .unwrap_or(Fp::zero());
            siblings.push(sib);

            let (left, right) = if pos % 2 == 0 {
                (current, sib)
            } else {
                (sib, current)
            };
            current = poseidon_hash(&[left, right]);

            let next_pos = pos >> 1;
            let next_level = &mut self.levels[(l + 1) as usize];
            if next_pos as usize == next_level.len() {
                next_level.push(current);
            } else {
                next_level[next_pos as usize] = current;
            }
        }

        self.leaf_count += 1;
        let root = current;
        (root, MembershipProof { index, siblings })
    }

    /// Verify a membership proof against a root.
    ///
    /// Returns `false` (fail closed) if the proof's sibling count doesn't match
    /// the tree depth, the index exceeds `2^depth`, or the hash chain doesn't
    /// reproduce the root.
    pub fn verify_proof(leaf: &Fp, proof: &MembershipProof, root: &Fp, depth: u32) -> bool {
        if proof.siblings.len() != depth as usize {
            return false;
        }
        if depth < 64 && proof.index >= (1u64 << depth) {
            return false;
        }
        let mut h = *leaf;
        for (l, sib) in proof.siblings.iter().enumerate() {
            let sib = *sib;
            h = if (proof.index >> l) & 1 == 0 {
                poseidon_hash(&[h, sib])
            } else {
                poseidon_hash(&[sib, h])
            };
        }
        h == *root
    }

    /// Take a snapshot of the tree state for persistence.
    ///
    /// `since` is the index (into `levels[0]`) of the first leaf to include in
    /// `recent_leaves`. Pass 0 to include all leaves.
    pub fn snapshot(&self, since: usize) -> TreeState {
        let start = since.min(self.leaf_count as usize);
        TreeState {
            root: self.root(),
            leaf_count: self.leaf_count,
            recent_leaves: self.levels[0][start..].to_vec(),
        }
    }
}

/// Serialize a tree root to its canonical 32-byte little-endian encoding.
pub fn root_to_bytes(root: &Fp) -> [u8; 32] {
    root.to_repr()
}

/// Deserialize a tree root from its 32-byte little-endian encoding.
/// Returns `None` on invalid field elements (fail closed).
pub fn bytes_to_root(bytes: &[u8; 32]) -> Option<Fp> {
    ct_option_to_opt(Fp::from_repr(*bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shielded::note::{commit, ShieldedNote};
    use pasta_curves::pallas::Scalar as Fq;

    /// A fixed test note for generating commitments.
    fn make_note(value: u64) -> ShieldedNote {
        ShieldedNote {
            value,
            owner_pk: [value as u8; 32],
            rho: Fq::from(value),
            rcm: Fq::from(value + 1),
        }
    }

    /// Compute the leaf for a note's commitment (mirrors `commitment_to_leaf`).
    fn note_leaf(note: &ShieldedNote) -> Fp {
        let c = commit(note);
        let coords = c.coordinates().expect("not infinity");
        poseidon_hash(&[coords.x().clone(), coords.y().clone()])
    }

    /// Append `n` distinct commitments to a fresh tree of the given depth.
    fn append_n(tree: &mut IncrementalMerkleTree, n: u64) -> Vec<(Fp, MembershipProof)> {
        (0..n)
            .map(|i| {
                let note = make_note(i);
                tree.append(&commit(&note))
            })
            .collect()
    }

    #[test]
    fn root_changes_on_append() {
        let mut tree = IncrementalMerkleTree::new(4);
        let (root1, _) = tree.append(&commit(&make_note(0)));
        let (root2, _) = tree.append(&commit(&make_note(1)));
        assert_ne!(root1, root2, "root must change after second append");
    }

    /// S5.3 restore constructor: `from_leaves` over the exact appended leaf
    /// sequence reproduces the appended tree bit-for-bit (root, leaf count),
    /// and membership proofs still verify against the restored root.
    #[test]
    fn from_leaves_reproduces_append_tree() {
        let mut tree = IncrementalMerkleTree::new(4);
        let mut appends = Vec::new();
        for i in 0..5u64 {
            appends.push(tree.append(&commit(&make_note(i))));
        }
        let leaves: Vec<Fp> = (0..5).map(|i| note_leaf(&make_note(i))).collect();
        let restored = IncrementalMerkleTree::from_leaves(4, &leaves);
        assert_eq!(restored.root(), tree.root(), "restored root must match");
        assert_eq!(restored.leaf_count(), tree.leaf_count(), "restored leaf count must match");
        let (_, proof) = &appends[4];
        assert!(
            IncrementalMerkleTree::verify_proof(&note_leaf(&make_note(4)), proof, &restored.root(), 4),
            "proof must verify against the restored root"
        );
    }

    #[test]
    fn from_leaves_empty() {
        let tree = IncrementalMerkleTree::from_leaves(4, &[]);
        assert_eq!(tree.root(), Fp::zero(), "empty restore is the zero root");
        assert_eq!(tree.leaf_count(), 0);
    }

    #[test]
    fn proof_verifies_against_new_root() {
        let mut tree = IncrementalMerkleTree::new(4);
        let (_root1, _) = tree.append(&commit(&make_note(0)));
        let (root2, proof2) = tree.append(&commit(&make_note(1)));
        let leaf2 = note_leaf(&make_note(1));
        assert!(
            IncrementalMerkleTree::verify_proof(&leaf2, &proof2, &root2, 4),
            "proof must verify against the new root"
        );
    }

    #[test]
    fn proof_fails_against_old_root() {
        let mut tree = IncrementalMerkleTree::new(4);
        let (root1, _) = tree.append(&commit(&make_note(0)));
        let (_root2, proof2) = tree.append(&commit(&make_note(1)));
        let leaf2 = note_leaf(&make_note(1));
        assert!(
            !IncrementalMerkleTree::verify_proof(&leaf2, &proof2, &root1, 4),
            "proof must NOT verify against the previous root"
        );
    }

    #[test]
    fn known_four_leaf_tree() {
        // Manually compute the depth-4 root for 4 leaves, accounting for
        // zero-padding of unfilled branches at levels 2, 3, and 4.
        let leaves: Vec<Fp> = (0..4).map(|i| note_leaf(&make_note(i))).collect();
        let zero = Fp::zero();

        // Level 1: hash pairs (0,1) and (2,3).
        let l1_0 = poseidon_hash(&[leaves[0], leaves[1]]);
        let l1_1 = poseidon_hash(&[leaves[2], leaves[3]]);
        // Level 2: the two level-1 nodes combine; the other level-2 slot is zero.
        let l2_0 = poseidon_hash(&[l1_0, l1_1]);
        // Level 3: l2_0 with its zero sibling.
        let l3_0 = poseidon_hash(&[l2_0, zero]);
        // Level 4 (root): l3_0 with its zero sibling.
        let expected_root = poseidon_hash(&[l3_0, zero]);

        let mut tree = IncrementalMerkleTree::new(4);
        append_n(&mut tree, 4);
        assert_eq!(
            tree.root(),
            expected_root,
            "tree root must match manually computed depth-4 4-leaf root"
        );
    }

    #[test]
    fn sibling_swap_discriminator() {
        let mut tree = IncrementalMerkleTree::new(4);
        let results = append_n(&mut tree, 4);
        let root = tree.root();

        // Take the proof for leaf 0 and swap one sibling.
        let (leaf0, proof0) = &results[0];
        let mut bad_proof = proof0.clone();
        bad_proof.siblings[0] = Fp::from(999u64); // swap first sibling

        assert!(
            !IncrementalMerkleTree::verify_proof(leaf0, &bad_proof, &root, 4),
            "swapping a sibling must invalidate the proof"
        );
    }

    #[test]
    fn duplicate_commitment_distinct_leaves() {
        let mut tree = IncrementalMerkleTree::new(4);
        let note = make_note(42);
        let (root1, proof1) = tree.append(&commit(&note));
        let (root2, proof2) = tree.append(&commit(&note));

        assert_eq!(proof1.index, 0, "first append is index 0");
        assert_eq!(proof2.index, 1, "second append is index 1");
        assert_ne!(root1, root2, "duplicate commitment must still change the root");

        let leaf = note_leaf(&note);
        // Each proof verifies against the root it was generated with.
        assert!(
            IncrementalMerkleTree::verify_proof(&leaf, &proof1, &root1, 4),
            "first proof must verify against its own root"
        );
        assert!(
            IncrementalMerkleTree::verify_proof(&leaf, &proof2, &root2, 4),
            "second proof must verify against its own root"
        );
        // The same leaf value at two distinct indices: both proofs are valid
        // against their respective roots even though the leaf is identical.
        assert_ne!(
            proof1.siblings, proof2.siblings,
            "distinct indices must have distinct sibling paths"
        );
    }

    #[test]
    fn empty_tree_root_is_zero() {
        let tree = IncrementalMerkleTree::new(4);
        assert_eq!(tree.root(), Fp::zero(), "empty tree root must be Fp::zero()");
    }

    #[test]
    fn depth_parameterization() {
        for depth in [4u32, 8u32] {
            let mut tree = IncrementalMerkleTree::new(depth);
            // Verify each proof against the root it was generated with.
            for i in 0..3u64 {
                let note = make_note(i);
                let (root, proof) = tree.append(&commit(&note));
                let leaf = note_leaf(&note);
                assert_eq!(
                    proof.siblings.len(),
                    depth as usize,
                    "proof length must equal depth"
                );
                assert!(
                    IncrementalMerkleTree::verify_proof(&leaf, &proof, &root, depth),
                    "proof must verify against its own root at depth {depth}"
                );
            }
        }
    }

    #[test]
    fn root_serialization_roundtrip() {
        let mut tree = IncrementalMerkleTree::new(4);
        append_n(&mut tree, 3);
        let root = tree.root();
        let bytes = root_to_bytes(&root);
        assert_eq!(bytes.len(), 32, "root encoding must be 32 bytes");
        let recovered = bytes_to_root(&bytes).expect("valid root must deserialize");
        assert_eq!(root, recovered, "roundtrip must preserve the root");
    }

    #[test]
    fn snapshot_recent_leaves() {
        let mut tree = IncrementalMerkleTree::new(4);
        append_n(&mut tree, 3);
        let snap = tree.snapshot(1);
        assert_eq!(snap.leaf_count, 3, "snapshot leaf_count must be 3");
        assert_eq!(snap.recent_leaves.len(), 2, "snapshot(1) must include leaves 1..3");
        assert_eq!(snap.root, tree.root(), "snapshot root must match tree root");
    }

    #[test]
    fn verify_proof_rejects_wrong_sibling_count() {
        let mut tree = IncrementalMerkleTree::new(4);
        let results = append_n(&mut tree, 2);
        let root = tree.root();
        let (leaf, proof) = &results[0];

        // Truncate the proof: wrong sibling count must be rejected.
        let mut bad = proof.clone();
        bad.siblings.pop();
        assert!(
            !IncrementalMerkleTree::verify_proof(leaf, &bad, &root, 4),
            "proof with wrong sibling count must be rejected"
        );
    }

    #[test]
    fn verify_proof_rejects_out_of_range_index() {
        let mut tree = IncrementalMerkleTree::new(4);
        append_n(&mut tree, 2);
        let root = tree.root();

        // Index 16 = 2^4 = out of range for depth 4.
        let bad_proof = MembershipProof {
            index: 16,
            siblings: vec![Fp::zero(); 4],
        };
        assert!(
            !IncrementalMerkleTree::verify_proof(&Fp::zero(), &bad_proof, &root, 4),
            "out-of-range index must be rejected"
        );
    }

    #[test]
    fn membership_proof_serde_roundtrip() {
        let mut tree = IncrementalMerkleTree::new(4);
        let results = append_n(&mut tree, 3);
        let (_, proof) = &results[1];

        let bytes = rmp_serde::to_vec(proof).expect("serialize MembershipProof");
        let recovered: MembershipProof =
            rmp_serde::from_slice(&bytes).expect("deserialize MembershipProof");
        assert_eq!(*proof, recovered, "MembershipProof serde roundtrip must preserve the proof");
    }

    #[test]
    fn tree_state_serde_roundtrip() {
        let mut tree = IncrementalMerkleTree::new(4);
        append_n(&mut tree, 3);
        let state = tree.snapshot(0);

        let bytes = rmp_serde::to_vec(&state).expect("serialize TreeState");
        let recovered: TreeState = rmp_serde::from_slice(&bytes).expect("deserialize TreeState");
        assert_eq!(state, recovered, "TreeState serde roundtrip must preserve state");
    }

    #[test]
    fn bytes_to_root_rejects_invalid_field_element() {
        // All-0xFF bytes represent a value >= p (invalid field element).
        let invalid = [0xFFu8; 32];
        assert!(
            bytes_to_root(&invalid).is_none(),
            "bytes >= p must fail to deserialize as Fp"
        );
    }
}
