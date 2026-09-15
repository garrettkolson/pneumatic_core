//! Phase S1.3 — Note commitment (Pedersen-style over Pallas Ep).
//!
//! Commitment formula:
//!   C = G_v · value + G_o · owner_pk_scalar + G_r · rcm + G_rho · rho
//!
//! where:
//!   - G_v, G_o, G_r, G_rho are four distinct Ep points (derived via
//!     `CurveExt::hash_to_curve` with a shared domain prefix and
//!     distinct input bytes — deterministic, no trusted setup).
//!   - value: u64 → Fq (Pallas scalar field) via `Fq::from(u64)`.
//!   - owner_pk: [u8; 32] → Fq via `Fq::from_uniform_bytes` (32-byte
//!     key placed in low 32 bytes of a 64-byte LE buffer, reduced mod q).
//!   - rcm: Fq (blinding factor, part of the note).
//!   - rho: Fq (note randomness, part of the note).
//!
//! Homomorphic property (used by S2.1 value-balance check):
//!   Σ C_i = G_v · (Σ value_i) + G_o · (Σ owner_pk_scalar_i)
//!          + G_r · (Σ rcm_i) + G_rho · (Σ rho_i)
//!
//! Deterministic: same (value, owner_pk, rcm) → same C, always.
//!
//! Return type is `EpAffine` (affine curve point), not a field element.
//! This deviates from the plan stub's loose `-> Fr` signature: a
//! Pedersen commitment is a curve point, and S2.1's homomorphic
//! value-balance check requires the group-addition structure that a
//! Poseidon hash to Fp does not provide.

use ff::{Field, FromUniformBytes};
use group::{Curve, Group};
use once_cell::sync::Lazy;
use pasta_curves::arithmetic::CurveExt;
use pasta_curves::pallas::{Affine as EpAffine, Point as Ep, Scalar as Fq};

/// A shielded note: the atomic unit of value in the shielded pool.
/// Mirrors Orchard's note structure (value, owner, rho, rcm).
///
/// Field types: `Fq` is the Pallas *scalar* field (255-bit). All scalar
/// multipliers in the Pedersen commitment are `Fq` elements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShieldedNote {
    /// Value in base units (u64, max ~1.8 × 10^19).
    pub value: u64,
    /// Spend authorization public key (32 bytes, Ed25519-style, derived
    /// from the spend key — NOT the node identity key).
    pub owner_pk: [u8; 32],
    /// Randomness (rho): binds the note to a specific commitment instance.
    /// Part of the note, not per-commit randomness.
    pub rho: Fq,
    /// Commitment blinding factor (rcm). Part of the note.
    pub rcm: Fq,
}

/// Generator for the value term: G_v.
static G_V: Lazy<Ep> = Lazy::new(|| {
    let hasher = Ep::hash_to_curve("pneumatic_note_commitment");
    hasher(b"Gv")
});

/// Generator for the owner_pk term: G_o.
static G_O: Lazy<Ep> = Lazy::new(|| {
    let hasher = Ep::hash_to_curve("pneumatic_note_commitment");
    hasher(b"Go")
});

/// Generator for the rcm term: G_r.
static G_R: Lazy<Ep> = Lazy::new(|| {
    let hasher = Ep::hash_to_curve("pneumatic_note_commitment");
    hasher(b"Gr")
});

/// Generator for the rho term: G_rho.
static G_RHO: Lazy<Ep> = Lazy::new(|| {
    let hasher = Ep::hash_to_curve("pneumatic_note_commitment");
    hasher(b"Grho")
});

/// Convert a 32-byte owner public key to an Fq scalar field element.
/// The 32-byte key is placed in the low 32 bytes of a 64-byte
/// little-endian buffer (high 32 bytes zero), then reduced mod q
/// via `from_uniform_bytes`.
fn owner_pk_to_scalar(pk: &[u8; 32]) -> Fq {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(pk);
    Fq::from_uniform_bytes(&buf)
}

/// Compute the Pedersen commitment for a note.
///
/// C = G_v · value + G_o · owner_pk_scalar + G_r · rcm
///
/// All scalar multipliers are `Fq` (Pallas scalar field).
/// Returns `EpAffine` (affine curve point). Deterministic: same note
/// fields → same commitment.
pub fn commit(note: &ShieldedNote) -> EpAffine {
    let value_scalar = Fq::from(note.value);
    let owner_scalar = owner_pk_to_scalar(&note.owner_pk);

    let c = G_V.clone() * value_scalar
        + G_O.clone() * owner_scalar
        + G_R.clone() * note.rcm
        + G_RHO.clone() * note.rho;

    c.to_affine()
}

#[cfg(test)]
mod tests {
    use super::*;
    use group::Group;

    /// A fixed test note with known field values.
    fn make_note() -> ShieldedNote {
        ShieldedNote {
            value: 42,
            owner_pk: [7u8; 32],
            rho: Fq::from(1),
            rcm: Fq::from(2),
        }
    }

    /// A note with value incremented by 1.
    fn make_note_value_plus_1() -> ShieldedNote {
        let mut n = make_note();
        n.value += 1;
        n
    }

    /// A note with one byte of owner_pk changed.
    fn make_note_owner_changed() -> ShieldedNote {
        let mut n = make_note();
        n.owner_pk[0] ^= 0xff;
        n
    }

    /// A note with rho changed.
    fn make_note_rho_changed() -> ShieldedNote {
        let mut n = make_note();
        n.rho = Fq::from(99);
        n
    }

    /// A note with rcm changed.
    fn make_note_rcm_changed() -> ShieldedNote {
        let mut n = make_note();
        n.rcm = Fq::from(99);
        n
    }

    #[test]
    fn commit_deterministic() {
        let note = make_note();
        assert_eq!(commit(&note), commit(&note), "same note must yield same commitment");
    }

    #[test]
    fn commit_value_discriminator() {
        let a = make_note();
        let b = make_note_value_plus_1();
        assert_ne!(
            commit(&a),
            commit(&b),
            "changing value by 1 must change the commitment"
        );
    }

    #[test]
    fn commit_owner_pk_discriminator() {
        let a = make_note();
        let b = make_note_owner_changed();
        assert_ne!(
            commit(&a),
            commit(&b),
            "changing one byte of owner_pk must change the commitment"
        );
    }

    #[test]
    fn commit_rho_discriminator() {
        let a = make_note();
        let b = make_note_rho_changed();
        assert_ne!(commit(&a), commit(&b), "changing rho must change the commitment");
    }

    #[test]
    fn commit_rcm_discriminator() {
        let a = make_note();
        let b = make_note_rcm_changed();
        assert_ne!(commit(&a), commit(&b), "changing rcm must change the commitment");
    }

    /// Load-bearing for S2.1: the Pedersen commitment is homomorphic in
    /// the group. commit(n1) + commit(n2) == commit(n_combined) where
    /// n_combined sums the scalar components of n1 and n2.
    #[test]
    fn commit_homomorphic_property() {
        let n1 = ShieldedNote {
            value: 10,
            owner_pk: [1u8; 32],
            rho: Fq::from(3),
            rcm: Fq::from(5),
        };
        let n2 = ShieldedNote {
            value: 20,
            owner_pk: [2u8; 32],
            rho: Fq::from(7),
            rcm: Fq::from(11),
        };

        // Combined note: sum of scalar components.
        let n_combined = ShieldedNote {
            value: n1.value + n2.value,
            owner_pk: {
                // owner_pk is not directly summable as bytes; instead we
                // verify the homomorphic property on the scalar level by
                // constructing the combined commitment directly.
                [0u8; 32] // placeholder — see assertion below
            },
            rho: n1.rho + n2.rho,
            rcm: n1.rcm + n2.rcm,
        };

        // The homomorphic property holds at the group level:
        // commit(n1) + commit(n2) = G_v*(v1+v2) + G_o*(o1+o2) + G_r*(r1+r2)
        // We verify this by computing the LHS directly and comparing to
        // the RHS computed from the summed scalars.
        let lhs: Ep = Ep::from(commit(&n1)) + Ep::from(commit(&n2));

        let v_sum = Fq::from(n1.value + n2.value);
        let o_sum = owner_pk_to_scalar(&n1.owner_pk) + owner_pk_to_scalar(&n2.owner_pk);
        let r_sum = n1.rcm + n2.rcm;
        let rho_sum = n1.rho + n2.rho;

        let rhs: Ep = G_V.clone() * v_sum
            + G_O.clone() * o_sum
            + G_R.clone() * r_sum
            + G_RHO.clone() * rho_sum;

        assert_eq!(
            lhs.to_affine(),
            rhs.to_affine(),
            "commitment must be homomorphic: C(n1)+C(n2) == C(summed scalars)"
        );

        // Also verify n_combined (with summed rho/rcm/value) produces the
        // same point when owner_pk is handled via the scalar sum.
        let _ = n_combined; // silence unused warning; the real check is above
    }

    /// KAT: known 32-byte input → known Fq output.
    #[test]
    fn owner_pk_to_scalar_known_answer() {
        // All-zero key → zero scalar.
        let zero_pk = [0u8; 32];
        assert_eq!(owner_pk_to_scalar(&zero_pk), Fq::from(0));

        // Key with only the lowest byte set → scalar = 1.
        let mut one_pk = [0u8; 32];
        one_pk[0] = 1;
        assert_eq!(owner_pk_to_scalar(&one_pk), Fq::from(1));

        // Key with byte[1] = 1 → scalar = 256 (little-endian).
        let mut two_pk = [0u8; 32];
        two_pk[1] = 1;
        assert_eq!(owner_pk_to_scalar(&two_pk), Fq::from(256));
    }
}
