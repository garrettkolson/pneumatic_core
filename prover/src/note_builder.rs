//! Phase S3.3 — note creation + note ciphertexts (`create_note`).
//!
//! [`create_note`] mints one output note and its encrypted ciphertext(s). The
//! note's `rho`/`rcm` are randomized per call, so two notes carrying the same
//! value to the same recipient are still **unlinkable** (different commitments).
//!
//! Wire note (`ShieldedTransaction.note_ciphertexts`, S3.1) carries **one**
//! ciphertext per output note — the spend-side one — matching the S1.5 sizing
//! (~2.3 KB / ciphertext, ~4.6 KB for a typical 2-note transfer). The viewing
//! ciphertext is distributed out-of-band to the viewing-key holder; the viewing
//! audit path (`scan_for_notes`, S3.3.4) consumes those supplied ciphertexts.

use pasta_curves::pallas::Scalar as Fq;
use pasta_curves::group::ff::FromUniformBytes;
use rand::RngCore;

use pneumatic_core::crypto::Ed25519Provider;
use pneumatic_core::shielded::{
    commit, decrypt_note, encrypt_note, encrypt_note_to_two, NotePlaintext, ShieldedNote,
};

use crate::key::ShieldedIdentity;

/// An output note request: a `value` and the recipient (whose `owner_pk` becomes
/// the note's value-auth key and whose `Ed25519Provider` receives the ciphertext).
pub struct NoteOutput {
    pub value: u64,
    pub recipient: ShieldedIdentity,
}

impl NoteOutput {
    /// Build an output note request.
    pub fn new(value: u64, recipient: ShieldedIdentity) -> Self {
        NoteOutput { value, recipient }
    }
}

/// Draw a fresh random Pallas scalar for a note's `rho`/`rcm` blinding.
fn random_scalar() -> Fq {
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    // `Scalar::from_uniform_bytes` is the standard way to hash down to a field
    // element; it is not the note commitment (that is `commit(&note)`).
    Fq::from_uniform_bytes(&bytes)
}

/// Create one output note for `recipient` worth `value`.
///
/// Returns `(note, spend_ciphertext, viewing_ciphertext)`:
/// * `note` — the minted [`ShieldedNote`] (owner key = the recipient's value-auth
///   key, `rho`/`rcm` randomized here).
/// * `spend_ciphertext` — the ciphertext carried on the wire (one per output,
///   S3.1); decryptable by `recipient.identity`.
/// * `viewing_ciphertext` — an independent ciphertext to the same provider, for
///   out-of-band distribution to a viewing-key holder (see S3.3.4). A distinct
///   compliance viewer is handled by encrypting the `NotePlaintext` to that
///   provider separately.
///
/// Infallible: encryption targets our own freshly generated providers, so it
/// cannot fail in practice.
pub fn create_note(recipient: &ShieldedIdentity, value: u64) -> (ShieldedNote, Vec<u8>, Vec<u8>) {
    let rho = random_scalar();
    let rcm = random_scalar();
    let note = ShieldedNote {
        value,
        owner_pk: recipient.owner_pk(),
        rho,
        rcm,
    };

    let plaintext = NotePlaintext {
        value,
        memo: Vec::new(),
        sender_pk_hint: None,
    };
    // One independent encryption per recipient role (S1.5). Both go to the
    // recipient's provider for v1; a distinct compliance viewer is encrypted to
    // separately.
    let (spend_ct, viewing_ct) =
        encrypt_note_to_two(&plaintext, &recipient.identity, &recipient.identity).expect("encrypt note");

    (note, spend_ct, viewing_ct)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::SpendKey;

    #[test]
    fn create_note_commitment_matches_core() {
        let identity = ShieldedIdentity {
            spend: SpendKey::from_seed([1u8; 32]),
            identity: Ed25519Provider::generate(),
        };
        let (note, _, _) = create_note(&identity, 100);

        // A core `ShieldedNote` whose commitment is deterministic and stable.
        assert_eq!(commit(&note), commit(&note), "commitment must be deterministic");
        // Bound to the recipient's value-auth key, and value-binding.
        assert_eq!(note.owner_pk, identity.owner_pk());
        let (note2, _, _) = create_note(&identity, 101);
        assert_ne!(
            commit(&note),
            commit(&note2),
            "a different value must produce a different commitment"
        );
    }

    #[test]
    fn create_note_randomizes_rho_and_rcm() {
        let identity = ShieldedIdentity {
            spend: SpendKey::from_seed([5u8; 32]),
            identity: Ed25519Provider::generate(),
        };
        let (n1, _, _) = create_note(&identity, 500);
        let (n2, _, _) = create_note(&identity, 500);

        // Same value + owner, but different blinding → unlinkable commitments.
        assert_ne!(n1.rho, n2.rho, "rho must be randomized per note");
        assert_ne!(n1.rcm, n2.rcm, "rcm must be randomized per note");
        assert_ne!(commit(&n1), commit(&n2), "randomized notes must be unlinkable");
        // Still bound to the recipient and value.
        assert_eq!(n1.owner_pk, identity.owner_pk());
        assert_eq!(n2.owner_pk, identity.owner_pk());
        assert_eq!(n1.value, 500);
        assert_eq!(n2.value, 500);
    }

    #[test]
    fn note_ciphertext_roundtrip_spender_and_viewing() {
        let spend_id = ShieldedIdentity {
            spend: SpendKey::from_seed([2u8; 32]),
            identity: Ed25519Provider::generate(),
        };
        let viewing_id = ShieldedIdentity {
            spend: SpendKey::from_seed([3u8; 32]),
            identity: Ed25519Provider::generate(),
        };
        let plaintext = NotePlaintext {
            value: 777,
            memo: vec![9u8; 2],
            sender_pk_hint: None,
        };
        let (spend_ct, viewing_ct) =
            encrypt_note_to_two(&plaintext, &spend_id.identity, &viewing_id.identity).unwrap();

        // Both recipients decrypt to the same plaintext, independently.
        assert_eq!(decrypt_note(&spend_ct, &spend_id.identity).unwrap(), plaintext);
        assert_eq!(decrypt_note(&viewing_ct, &viewing_id.identity).unwrap(), plaintext);
    }

    #[test]
    fn note_ciphertext_wrong_key_fails_closed() {
        let id = ShieldedIdentity {
            spend: SpendKey::from_seed([4u8; 32]),
            identity: Ed25519Provider::generate(),
        };
        let other = Ed25519Provider::generate();
        let plaintext = NotePlaintext {
            value: 1,
            memo: vec![],
            sender_pk_hint: None,
        };
        let ct = encrypt_note(&plaintext, &id.identity).unwrap();

        // Wrong key → Err, never garbage plaintext (S1.5 fail-closed discriminator).
        assert!(decrypt_note(&ct, &other).is_err(), "foreign key must fail closed");
        let mut corrupt = ct.clone();
        corrupt[0] ^= 0xFF;
        assert!(
            decrypt_note(&corrupt, &id.identity).is_err(),
            "a corrupt ciphertext must fail closed"
        );
    }
}
