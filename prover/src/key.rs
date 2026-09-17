//! Phase S3.3 — the `pneumatic_prover` spend / viewing key model.
//!
//! A [`SpendKey`] is a 32-byte seed that derives, deterministically:
//!
//! * `spend_secret` — the private side, fed to
//!   `pneumatic_core::shielded::nullifier` and used to witness spend authority
//!   in the Action circuit (S2.1).
//! * `owner_pk` — the public 32-byte **value-authorization** key embedded in a
//!   note's Pedersen commitment (`ShieldedNote.owner_pk`, note.rs:49). This is a
//!   *value* key, not a node identity key, and is independent of the
//!   `Ed25519Provider` used to encrypt notes to the holder (note.rs:214).
//! * `viewing_key` — a 32-byte key that can decrypt note ciphertexts (audit /
//!   compliance, roadmap 2.6) but cannot spend (it lacks the spend secret).
//!
//! Derivation is pinned below so the off-circuit contract is stable and
//! testable — the in-circuit `owner_pk == H(spend_secret)` binding is the Action
//! circuit's responsibility (S2.1), flagged as an open item in
//! `phase-s3-3-prover-crate.md`:
//!
//! ```text
//! spend_secret = sha256("pneumatic-shielded/v1/spend-secret" || seed)
//! owner_pk     = sha256("pneumatic-shielded/v1/owner-pk"     || spend_secret)
//! viewing_key  = sha256("pneumatic-shielded/v1/viewing-key"  || seed)
//! ```
//!
//! The plan's "Ed25519 keys are fine here" is honored by *allowing* the
//! spend-secret / owner-pk pair to be an Ed25519 `(sk, pk)`; the derivation
//! above is kept because the commitment needs only 32 arbitrary bytes and an
//! in-circuit Ed25519 verify is too expensive for v1.

use sha2::{Digest, Sha256};

use pneumatic_core::crypto::Ed25519Provider;

/// A 32-byte seed. Every value derived from a seed is a pure function of it.
#[derive(Clone, Debug)]
pub struct SpendKey {
    seed: [u8; 32],
}

impl SpendKey {
    /// Build a spend key from a raw 32-byte seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        SpendKey { seed }
    }

    /// The private spend side: drives nullifier derivation and spend authority.
    pub fn spend_secret(&self) -> [u8; 32] {
        derive(b"pneumatic-shielded/v1/spend-secret", &self.seed)
    }

    /// The public value-authorization key embedded in a note commitment.
    ///
    /// `owner_pk == H("pneumatic-shielded/v1/owner-pk" || spend_secret)` — the
    /// off-circuit contract the prover satisfies (see the
    /// `owner_pk_is_derivable_from_spend_secret` test).
    pub fn owner_pk(&self) -> [u8; 32] {
        derive(b"pneumatic-shielded/v1/owner-pk", &self.spend_secret())
    }

    /// The viewing key: decrypts note ciphertexts for audit/compliance, cannot
    /// spend (it shares no material with `spend_secret`).
    pub fn viewing_key(&self) -> [u8; 32] {
        derive(b"pneumatic-shielded/v1/viewing-key", &self.seed)
    }
}

/// Deterministic 32-byte derivation of `sha256(hash_input || input)`.
fn derive(hash_input: &[u8], input: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(hash_input);
    hasher.update(input);
    let out = hasher.finalize();
    let mut out_bytes = [0u8; 32];
    out_bytes.copy_from_slice(&out);
    out_bytes
}

/// A wallet recipient: the value-auth [`SpendKey`] bundled with the identity
/// provider used to encrypt notes to them (S3.3 §4b). The client constructs this
/// from its own key material; the crate only consumes it.
///
/// Deliberately does **not** derive `Clone`/`Debug`: `Ed25519Provider` wraps
/// `RwLock`s and implements neither, and neither is needed here.
pub struct ShieldedIdentity {
    /// Value-authorization key (drives `owner_pk` + nullifier).
    pub spend: SpendKey,
    /// Hybrid identity (`Ed25519Provider`) used to encrypt output notes to them.
    pub identity: Ed25519Provider,
}

impl ShieldedIdentity {
    /// The recipient's value-authorization public key — the note commitment key.
    ///
    /// Reads through to [`SpendKey::owner_pk`], so a wallet that later swaps the
    /// derivation scheme can only do so in one place.
    pub fn owner_pk(&self) -> [u8; 32] {
        self.spend.owner_pk()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_derivation_is_deterministic() {
        let seed = [7u8; 32];
        let a = SpendKey::from_seed(seed);
        let b = SpendKey::from_seed(seed);
        assert_eq!(a.spend_secret(), b.spend_secret());
        assert_eq!(a.owner_pk(), b.owner_pk());
        assert_eq!(a.viewing_key(), b.viewing_key());
    }

    #[test]
    fn changing_seed_changes_all_derived_values() {
        // Discriminator: the derived values are a pure function of the seed, so a
        // different seed must change all three — not just one.
        let a = SpendKey::from_seed([7u8; 32]);
        let b = SpendKey::from_seed([8u8; 32]);
        assert_ne!(a.spend_secret(), b.spend_secret());
        assert_ne!(a.owner_pk(), b.owner_pk());
        assert_ne!(a.viewing_key(), b.viewing_key());
    }

    #[test]
    fn owner_pk_is_derivable_from_spend_secret() {
        // Off-circuit contract (S3.3.1): owner_pk == H("…/owner-pk" || spend_secret)
        // and viewing_key == H("…/viewing-key" || seed). Recompute and compare —
        // proves the derivation is exactly as documented, not a hidden constant.
        let sk = SpendKey::from_seed([42u8; 32]);

        let expected_owner = derive(b"pneumatic-shielded/v1/owner-pk", &sk.spend_secret());
        assert_eq!(sk.owner_pk(), expected_owner);

        let expected_viewing = derive(b"pneumatic-shielded/v1/viewing-key", &sk.seed);
        assert_eq!(sk.viewing_key(), expected_viewing);

        // The three derived values are mutually distinct for a fresh seed — a
        // spender, a viewer, and the commitment key must not collide.
        assert_ne!(sk.spend_secret(), sk.owner_pk());
        assert_ne!(sk.spend_secret(), sk.viewing_key());
        assert_ne!(sk.owner_pk(), sk.viewing_key());
    }
}
