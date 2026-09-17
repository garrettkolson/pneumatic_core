//! Phase S3.3 — `scan_for_notes`: the viewing-key compliance/audit path.
//!
//! A compliance holder of a viewing key holds **ciphertexts only** (they never
//! see `rho`/`rcm`, commitment, or the note's spending key). `scan_for_notes`
//! decrypts each supplied ciphertext with the viewing key and returns the
//! `NotePlaintext`(s). It never reconstructs a `ShieldedNote`: the audit path
//! recovers the note's *value + memo*, never its commitment blinding randomness.
//!
//! Fail-closed: a ciphertext that a viewing provider did not receive decrypts to
//! a GCM tag failure, which `scan_for_notes` surfaces as `Err` (it returns on the
//! first such failure), never garbage.

use pneumatic_core::crypto::Ed25519Provider;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::shielded::{decrypt_note, NotePlaintext};

/// Decrypt the supplied viewing-key ciphertexts with `viewing_provider` and
/// return the recovered `NotePlaintext`(s).
///
/// Returns `Err` on the first ciphertext that cannot be decrypted with
/// `viewing_provider` (wrong viewing key, corrupted ciphertext, or a ciphertext
/// never intended for this provider) — a non-matching ciphertext is rejected,
/// never silently skipped.
pub fn scan_for_notes(
    ciphertexts: &[Vec<u8>],
    viewing_provider: &Ed25519Provider,
) -> Result<Vec<NotePlaintext>, PneumaticError> {
    // Collecting into `Result<Vec<_>, _>` short-circuits on the first `Err`, so
    // the scan fails closed rather than silently dropping a bad ciphertext.
    ciphertexts
        .iter()
        .map(|ct| decrypt_note(ct, viewing_provider))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pneumatic_core::shielded::encrypt_note_to_two;

    /// The audit path returns a `NotePlaintext` (value + memo + optional
    /// sender hint) — never a `ShieldedNote`. There is no `rho`/`rcm` on a
    /// `NotePlaintext` at all (compile-time), so the holder can never recover
    /// the note's commitment blinding randomness.
    #[test]
    fn scan_for_notes_returns_plaintext_not_note() {
        let spending_provider = Ed25519Provider::generate();
        let viewing_provider = Ed25519Provider::generate();
        let pt = NotePlaintext {
            value: 42,
            memo: b"compliance memo".to_vec(),
            sender_pk_hint: None,
        };

        // One independent encryption per role; the viewing ciphertext is what the
        // audit holder receives.
        let (_spend_ct, viewing_ct) =
            encrypt_note_to_two(&pt, &spending_provider, &viewing_provider).expect("encrypt");

        let result = scan_for_notes(&[viewing_ct], &viewing_provider).expect("scan");

        assert_eq!(result.len(), 1, "one ciphertext yields one plaintext");
        assert_eq!(result[0].value, 42, "the audit holder recovers the value");
        assert_eq!(
            result[0].memo, b"compliance memo",
            "the audit holder recovers the memo"
        );

        // The result type is `NotePlaintext` — destructuring to its real fields is
        // the assertion: value + memo + sender hint only. There is no `rho`/`rcm`
        // (nor a commitment) to recover.
        let NotePlaintext {
            value,
            memo,
            sender_pk_hint,
        } = &result[0];
        let _ = (value, memo, sender_pk_hint);
    }

    /// A viewing provider that did not receive the ciphertext must be rejected
    /// (fail closed) — a wrong viewing key decrypts to a GCM tag failure.
    #[test]
    fn scan_for_notes_wrong_viewing_key_rejected() {
        let viewing_provider = Ed25519Provider::generate();
        let attacker = Ed25519Provider::generate();
        let pt = NotePlaintext {
            value: 42,
            memo: Vec::new(),
            sender_pk_hint: None,
        };

        let (_spend_ct, viewing_ct) =
            encrypt_note_to_two(&pt, &Ed25519Provider::generate(), &viewing_provider).expect("encrypt");

        // The audit holder is a *different* provider than the one that received
        // the ciphertext: this must fail closed, never return garbage.
        let result = scan_for_notes(&[viewing_ct], &attacker);
        assert!(
            result.is_err(),
            "a viewing provider that did not receive the ciphertext must be rejected"
        );
    }
}
