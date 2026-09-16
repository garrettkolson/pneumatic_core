//! Phase S1.3 — Note commitment (Pedersen-style over Pallas Ep).
//! Phase S1.4 — Nullifier derivation (Poseidon over Pallas Fp).
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

use ff::{Field, FromUniformBytes, PrimeField};
use group::{Curve, Group};
use once_cell::sync::Lazy;
use pasta_curves::arithmetic::CurveExt;
use pasta_curves::pallas::{Affine as EpAffine, Base as Fp, Point as Ep, Scalar as Fq};
use serde::{Deserialize, Serialize};

use crate::crypto::{AsymCryptoProvider, Ed25519Provider};
use crate::errors::PneumaticError;

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
pub fn owner_pk_to_scalar(pk: &[u8; 32]) -> Fq {
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

// ── Phase S1.4: Nullifier derivation ──────────────────────────────────────────

/// Domain separator for nullifier derivation. Distinct from the commitment
/// generator domain (`"pneumatic_note_commitment"`).
static NULLIFIER_DOMAIN: Lazy<Fp> = Lazy::new(|| Fp::from(0x01u64));

/// Convert a 32-byte value to an `Fp` field element via uniform-byte reduction.
/// The 32 bytes are placed in the low 32 bytes of a 64-byte little-endian
/// buffer (high 32 bytes zero), then reduced mod p.
fn bytes32_to_fp(bytes: &[u8; 32]) -> Fp {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(bytes);
    Fp::from_uniform_bytes(&buf)
}

/// Derive the nullifier for a note given the spender's secret key.
///
/// `nullifier = poseidon_hash([domain, spend_key_fp, rho_fp]).to_repr()`
///
/// Properties:
/// - **Deterministic**: same (note, spend_key) → same nullifier, always.
/// - **rho-bound**: two spends of the same note (same rho) yield the same
///   nullifier; different notes (different rho) yield different nullifiers.
/// - **Spend-key mixed**: the spend key is mixed into the hash, so the
///   nullifier reveals nothing about which note was spent without the key.
/// - **Unlinkable to commitment**: given (commitment, spend_key), the nullifier
///   cannot be derived without rho, because the Pedersen commitment is
///   one-way (discrete-log hardness).
///
/// The nullifier depends only on (spend_key, rho) — not on value or owner_pk —
/// so it identifies the note *instance* and the authorized spender, not the
/// note's contents.
pub fn nullifier(note: &ShieldedNote, spend_key: &[u8; 32]) -> [u8; 32] {
    let spend_key_fp = bytes32_to_fp(spend_key);
    let rho_bytes = note.rho.to_repr();
    let rho_fp = bytes32_to_fp(&rho_bytes);

    let h = crate::shielded::poseidon::poseidon_hash(&[*NULLIFIER_DOMAIN, spend_key_fp, rho_fp]);
    h.to_repr()
}

// ── Phase S1.5: Note encryption (reuse hybrid crypto) ─────────────────────────

/// The plaintext payload of a shielded note, encrypted to the recipient(s)
/// before being placed on the wire. Serialized with rmp-serde before
/// encryption.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotePlaintext {
    /// Value in base units.
    pub value: u64,
    /// Arbitrary memo bytes (application-defined).
    pub memo: Vec<u8>,
    /// Optional sender public-key hint (32 bytes) for the recipient to
    /// identify the sender without breaking unlinkability.
    pub sender_pk_hint: Option<[u8; 32]>,
}

/// Serialize a `NotePlaintext` to MsgPack bytes.
fn note_plaintext_to_bytes(pt: &NotePlaintext) -> Result<Vec<u8>, PneumaticError> {
    crate::encoding::serialize_to_bytes_rmp(pt)
        .map_err(|e| PneumaticError::CryptoError(format!("NotePlaintext serialization failed: {}", e)))
}

/// Deserialize MsgPack bytes to a `NotePlaintext`.
fn bytes_to_note_plaintext(bytes: &[u8]) -> Result<NotePlaintext, PneumaticError> {
    crate::encoding::deserialize_rmp_to::<NotePlaintext>(&bytes.to_vec())
        .map_err(|e| PneumaticError::CryptoError(format!("NotePlaintext deserialization failed: {}", e)))
}

/// Encrypt a note plaintext to a single recipient using the hybrid
/// `(X25519 · ML-KEM-768)` scheme.
///
/// Returns the ciphertext bytes. The recipient decrypts with
/// [`decrypt_note`] using their own `Ed25519Provider`.
pub fn encrypt_note(
    plaintext: &NotePlaintext,
    recipient: &Ed25519Provider,
) -> Result<Vec<u8>, PneumaticError> {
    let bytes = note_plaintext_to_bytes(plaintext)?;
    let x25519_pk = recipient.x25519_public_key()?;
    let mlkem_pk = recipient.mlkem_public_key()?;
    recipient.encrypt_to(&x25519_pk, &mlkem_pk, bytes)
}

/// Decrypt a note ciphertext produced by [`encrypt_note`] and deserialize the
/// `NotePlaintext`.
///
/// Returns `Err(CryptoError)` on GCM tag failure (wrong key, corrupted
/// ciphertext) — never returns garbage plaintext.
pub fn decrypt_note(
    ciphertext: &[u8],
    recipient: &Ed25519Provider,
) -> Result<NotePlaintext, PneumaticError> {
    let bytes = recipient.decrypt_from(ciphertext.to_vec())?;
    bytes_to_note_plaintext(&bytes)
}

/// Encrypt a note plaintext independently to two recipients (spend-key holder
/// and viewing-key holder). Returns `(spend_ct, viewing_ct)`.
///
/// The two ciphertexts are independent hybrid encryptions — no key-sharing
/// between them. A holder of either key can decrypt their own ciphertext;
/// cross-decryption fails closed.
pub fn encrypt_note_to_two(
    plaintext: &NotePlaintext,
    spend_recipient: &Ed25519Provider,
    viewing_recipient: &Ed25519Provider,
) -> Result<(Vec<u8>, Vec<u8>), PneumaticError> {
    let spend_ct = encrypt_note(plaintext, spend_recipient)?;
    let viewing_ct = encrypt_note(plaintext, viewing_recipient)?;
    Ok((spend_ct, viewing_ct))
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

    // ── Phase S1.4: Nullifier tests ───────────────────────────────────────────

    fn make_spend_key() -> [u8; 32] {
        [0xAB; 32]
    }

    fn make_alt_spend_key() -> [u8; 32] {
        [0xCD; 32]
    }

    #[test]
    fn nullifier_deterministic() {
        let note = make_note();
        let key = make_spend_key();
        assert_eq!(
            nullifier(&note, &key),
            nullifier(&note, &key),
            "same (note, spend_key) must yield same nullifier"
        );
    }

    #[test]
    fn nullifier_rho_discriminator() {
        let a = make_note();
        let b = make_note_rho_changed();
        let key = make_spend_key();
        assert_ne!(
            nullifier(&a, &key),
            nullifier(&b, &key),
            "different rho must yield different nullifier"
        );
    }

    #[test]
    fn nullifier_spend_key_discriminator() {
        let note = make_note();
        assert_ne!(
            nullifier(&note, &make_spend_key()),
            nullifier(&note, &make_alt_spend_key()),
            "different spend_key must yield different nullifier"
        );
    }

    #[test]
    fn nullifier_independent_of_value() {
        let a = make_note();
        let b = make_note_value_plus_1();
        let key = make_spend_key();
        assert_eq!(
            nullifier(&a, &key),
            nullifier(&b, &key),
            "nullifier must not depend on value"
        );
    }

    #[test]
    fn nullifier_independent_of_owner_pk() {
        let a = make_note();
        let b = make_note_owner_changed();
        let key = make_spend_key();
        assert_eq!(
            nullifier(&a, &key),
            nullifier(&b, &key),
            "nullifier must not depend on owner_pk"
        );
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

    // ── Phase S1.5: Note encryption tests ─────────────────────────────────────

    fn make_note_plaintext() -> NotePlaintext {
        NotePlaintext {
            value: 1000,
            memo: b"hello note".to_vec(),
            sender_pk_hint: Some([0x42; 32]),
        }
    }

    #[test]
    fn note_encrypt_decrypt_roundtrip_spend_key() {
        let provider = Ed25519Provider::generate();
        let pt = make_note_plaintext();
        let ct = encrypt_note(&pt, &provider).expect("encrypt_note must succeed");
        let decrypted = decrypt_note(&ct, &provider).expect("decrypt_note must succeed");
        assert_eq!(pt, decrypted, "roundtrip must preserve NotePlaintext");
    }

    #[test]
    fn note_encrypt_decrypt_roundtrip_viewing_key() {
        let spend_provider = Ed25519Provider::generate();
        let viewing_provider = Ed25519Provider::generate();
        let pt = make_note_plaintext();
        let (_, viewing_ct) =
            encrypt_note_to_two(&pt, &spend_provider, &viewing_provider)
                .expect("encrypt_note_to_two must succeed");
        let decrypted = decrypt_note(&viewing_ct, &viewing_provider)
            .expect("viewing-key decrypt must succeed");
        assert_eq!(pt, decrypted, "viewing-key roundtrip must preserve NotePlaintext");
    }

    #[test]
    fn note_wrong_recipient_fails_closed() {
        let provider_a = Ed25519Provider::generate();
        let provider_b = Ed25519Provider::generate();
        let pt = make_note_plaintext();
        let ct = encrypt_note(&pt, &provider_a).expect("encrypt_note must succeed");
        let result = decrypt_note(&ct, &provider_b);
        assert!(
            result.is_err(),
            "decryption with wrong recipient must fail (GCM tag mismatch)"
        );
    }

    #[test]
    fn note_corrupt_ciphertext_fails_closed() {
        let provider = Ed25519Provider::generate();
        let pt = make_note_plaintext();
        let mut ct = encrypt_note(&pt, &provider).expect("encrypt_note must succeed");
        // Flip the last byte (inside the GCM tag).
        let last = ct.len() - 1;
        ct[last] ^= 0xff;
        let result = decrypt_note(&ct, &provider);
        assert!(
            result.is_err(),
            "corrupted ciphertext must fail (GCM tag mismatch), never return garbage"
        );
    }

    #[test]
    fn note_two_recipient_independence() {
        let spend_provider = Ed25519Provider::generate();
        let viewing_provider = Ed25519Provider::generate();
        let pt = make_note_plaintext();
        let (spend_ct, viewing_ct) =
            encrypt_note_to_two(&pt, &spend_provider, &viewing_provider)
                .expect("encrypt_note_to_two must succeed");

        // Ciphertexts must differ (independent encryptions, different keys).
        assert_ne!(
            spend_ct, viewing_ct,
            "two independent encryptions to different recipients must produce distinct ciphertexts"
        );

        // Each recipient can decrypt their own ciphertext.
        assert_eq!(
            decrypt_note(&spend_ct, &spend_provider).expect("spend decrypt"),
            pt
        );
        assert_eq!(
            decrypt_note(&viewing_ct, &viewing_provider).expect("viewing decrypt"),
            pt
        );

        // Cross-decrypt must fail.
        assert!(
            decrypt_note(&spend_ct, &viewing_provider).is_err(),
            "spend ciphertext must not decrypt with viewing key"
        );
        assert!(
            decrypt_note(&viewing_ct, &spend_provider).is_err(),
            "viewing ciphertext must not decrypt with spend key"
        );
    }

    #[test]
    fn note_empty_memo_roundtrip() {
        let provider = Ed25519Provider::generate();
        let pt = NotePlaintext {
            value: 0,
            memo: vec![],
            sender_pk_hint: None,
        };
        let ct = encrypt_note(&pt, &provider).expect("encrypt_note must succeed");
        let decrypted = decrypt_note(&ct, &provider).expect("decrypt_note must succeed");
        assert_eq!(pt, decrypted, "empty-memo NotePlaintext must roundtrip");
    }
}
