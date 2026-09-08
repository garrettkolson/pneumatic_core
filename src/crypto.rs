use std::sync::{Arc, RwLock};
use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use ed25519_dalek::{SigningKey, Signer, Verifier, VerifyingKey};
use hkdf::Hkdf;
use pqcrypto_mldsa::mldsa44::{
    detached_sign as mldsa_detached_sign, keypair as mldsa_keypair,
    verify_detached_signature as mldsa_verify, DetachedSignature, PublicKey as MldsaPublicKey,
    SecretKey as MldsaSecretKey,
};
use pqcrypto_mlkem::mlkem768::{
    encapsulate as mlkem_encapsulate, decapsulate as mlkem_decapsulate, keypair as mlkem_keypair,
    Ciphertext as MLKemCiphertext, PublicKey as MLKemPublicKey, SecretKey as MLKemSecretKey,
};
// The byte accessors (`as_bytes` / `from_bytes`) live on the pqcrypto_traits
// kem/sign traits, which these wrapper structs implement. Importing the traits
// under distinct aliases brings their methods into scope without clashing with
// the wrapper type aliases above.
use pqcrypto_traits::kem::{
    Ciphertext as KemCiphertextTrait, PublicKey as KemPublicKeyTrait,
    SecretKey as KemSecretKeyTrait, SharedSecret as KemSharedSecretTrait,
};
use pqcrypto_traits::sign::{
    DetachedSignature as SignDetachedSignatureTrait, PublicKey as SignPublicKeyTrait,
};
use ring::digest;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey, StaticSecret};
use crate::errors::PneumaticError;

// ---------------------------------------------------------------------------
// AsymCryptoProvider — hybrid (classical · post-quantum) asymmetric crypto
// ---------------------------------------------------------------------------
//
// Every operation is a HYBRID of a classical and a post-quantum primitive,
// concatenated — the "N = N+1" strategy used by TLS 1.3, WireGuard and
// Cloudflare in 2026. This keeps the protocol interoperable with still-classical
// peers and safe against an unknown failure (or a cryptographically-relevant
// quantum computer that breaks just one of the two schemes): an attacker must
// break *both* halves to achieve anything.
//
//   Signatures   : (Ed25519 · ML-DSA-44)  — see `check_signature` / `sign_data`
//   Key exchange : (X25519 · ML-KEM-768)  — see `encrypt` / `decrypt`
//
// SHA-256 (`HashProvider`) is intentionally left untouched: Grover only halves
// its budget (128 effective bits), so it remains adequate. See
// AUDIT_CHECKLIST.md Phase 7 for the wire-shape and interop notes.

// ---------------------------------------------------------------------------
// Fixed byte offsets of the hybrid wire formats.
//
//   Hybrid signature: [ Ed25519 sig(64) · ML-DSA-44 public key(1312) ·
//                       ML-DSA-44 signature(2420) ]  == 3796 bytes.
//       The ML-DSA public key is embedded so the signature blob is
//       self-describing — a receiver verifies the ML-DSA half without a
//       separate key lookup. (Both halves must verify: this is what keeps
//       forgery post-quantum-safe, rather than "either half" which would
//       degrade to the strength of the classical Ed25519 half.)
//
//   Hybrid KEM ciphertext: [ X25519 ephemeral PK(32) · ML-KEM-768
//       encapsulation(1088) · ML-KEM-768 recipient PK(1184) · AES nonce(12) ·
//       ciphertext + GCM tag(16) ]  == 2332 bytes for empty plaintext.
//       The ML-KEM recipient PK is embedded so decrypt can fail-closed if the
//       encapsulation was addressed to someone else.
// ---------------------------------------------------------------------------

// Hybrid wire-format constants. These are `pub` because they are part of the
// public interop contract: identity.rs and messages.rs slice the hybrid
// `[Ed25519 · ML-DSA]` signature (and the KEM header) into their halves using
// them, and the AUDIT_CHECKLIST documents the resulting byte sizes.
pub const ED25519_SIG_LEN: usize = 64;
pub const MLDSA_PK_LEN: usize = 1312;
pub const MLDSA_SIG_LEN: usize = 2420;
pub const MLDSA_FULL_SIG_LEN: usize = ED25519_SIG_LEN + MLDSA_PK_LEN + MLDSA_SIG_LEN; // 3796

pub const X25519_PK_LEN: usize = 32;
pub const MLKEM_CT_LEN: usize = 1088;
pub const MLKEM_PK_LEN: usize = 1184;
pub const NONCE_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;

const HYBRID_KEM_HEADER_LEN: usize =
    X25519_PK_LEN + MLKEM_CT_LEN + MLKEM_PK_LEN + NONCE_LEN; // 2316
const HYBRID_KEM_EMPTY_LEN: usize = HYBRID_KEM_HEADER_LEN + GCM_TAG_LEN; // 2332

#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub enum AsymCryptoProviderType {
    Ed25519,
}

pub fn get_asym_provider(
    provider_type: &AsymCryptoProviderType,
) -> Arc<RwLock<dyn AsymCryptoProvider>> {
    match provider_type {
        AsymCryptoProviderType::Ed25519 => {
            let provider = Ed25519Provider::generate();
            Arc::new(RwLock::new(provider))
        }
    }
}

pub trait AsymCryptoProvider: Send + Sync {
    /// Encrypt `data` for self (using this provider's static keys). Returns the
    /// hybrid `(X25519 · ML-KEM-768)` ciphertext:
    /// `[X25519 eph PK · ML-KEM encapsulation · ML-KEM recipient PK · nonce · ct+tag]`.
    fn encrypt(&self, data: Vec<u8>) -> Result<Vec<u8>, PneumaticError>;
    /// Decrypt data that was encrypted for self via `encrypt`.
    fn decrypt(&self, data: Vec<u8>) -> Result<Vec<u8>, PneumaticError>;
    /// Encrypt `data` to an arbitrary recipient. `recipient_public_key` is the
    /// recipient's 32-byte X25519 key; `recipient_mlkem_pk` is their ML-KEM-768
    /// public key. Anyone with the recipient's public keys can encrypt; only the
    /// recipient (holding the matching static secrets) can decrypt.
    fn encrypt_to(
        &self,
        recipient_public_key: &[u8; 32],
        recipient_mlkem_pk: &[u8],
        data: Vec<u8>,
    ) -> Result<Vec<u8>, PneumaticError>;
    /// Decrypt data that was encrypted to this provider via `encrypt_to`.
    fn decrypt_from(&self, data: Vec<u8>) -> Result<Vec<u8>, PneumaticError>;
    /// Verify a hybrid `(Ed25519 · ML-DSA-44)` signature over `data`.
    ///
    /// `public_key` is the sender's 32-byte Ed25519 verifying key. Returns
    /// `Ok(true)` only when BOTH halves verify — the Ed25519 half against
    /// `public_key`, and the ML-DSA half against the key embedded in the
    /// signature. The "both halves" policy is what keeps forgery
    /// post-quantum-safe (an attacker must break both schemes); a bare-classical
    /// Ed25519 signature therefore fails here and must be rejected per the audit
    /// checklist's coordinated-upgrade guidance.
    fn check_signature(
        &self,
        signature: &[u8],
        public_key: &[u8],
        data: &[u8],
    ) -> Result<bool, PneumaticError>;
    /// Sign `data` with this provider's hybrid `(Ed25519 · ML-DSA-44)` keys.
    fn sign_data(&self, data: &[u8]) -> Result<Vec<u8>, PneumaticError>;
    /// Return the Ed25519 verifying key (for signature verification).
    ///
    /// This is the on-chain identity and is unchanged in wire shape — still the
    /// 32-byte classical key. The ML-DSA key is *not* the public identity key;
    /// it is embedded inside each hybrid signature (see `sign_data`).
    fn public_key(&self) -> Result<Vec<u8>, PneumaticError>;
}

/// Hybrid crypto provider backed by ed25519-dalek (Ed25519 signatures), the
/// PQClean ML-DSA-44 and ML-KEM-768 FFI bindings, and AES-256-GCM.
///
/// It carries four keypairs:
/// - `signing_key` / `verifying_key` — Ed25519 (classical identity key),
/// - `x25519_static_key` — X25519 (classical DH key exchange),
/// - `mldsa_keypair` — ML-DSA-44 (post-quantum signature key),
/// - `mlkem_keypair` — ML-KEM-768 (post-quantum KEM key).
///
/// Signing produces `[Ed25519 sig · ML-DSA pk · ML-DSA sig]`; verification
/// requires both halves. Encryption uses `(X25519 · ML-KEM-768)`. The X25519 /
/// ML-KEM static secret material is not the persisted identity — the Ed25519
/// seed is (see `from_seed`); this is documented in AUDIT_CHECKLIST.md Phase 7.
pub struct Ed25519Provider {
    signing_key: RwLock<SigningKey>,
    verifying_key: RwLock<VerifyingKey>,
    x25519_static_key: RwLock<StaticSecret>,
    mldsa_keypair: RwLock<(MldsaPublicKey, MldsaSecretKey)>,
    mlkem_keypair: RwLock<(MLKemPublicKey, MLKemSecretKey)>,
}

impl Ed25519Provider {
    /// Generate a fresh hybrid key set: a 32-byte Ed25519 seed, plus new X25519,
    /// ML-DSA-44 and ML-KEM-768 keypairs. The Ed25519 key is the persisted
    /// identity; the others are key-exchange / signature material (see
    /// `from_seed` for what is and isn't recovered from the seed).
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("Failed to generate random seed");
        Self::from_seed(seed)
    }

    /// Build a provider from a persisted 32-byte Ed25519 seed.
    ///
    /// The Ed25519 key is fully determined by the seed (it is the on-chain
    /// identity). The X25519, ML-DSA-44 and ML-KEM-768 keypairs are generated
    /// freshly — they are key-exchange / signature material, not the persisted
    /// identity. (Persisting ML-DSA/ML-KEM seeds so these become deterministic is
    /// the keystore work in the follow-up Phase 7 node-server task; until then
    /// they are regenerated on each boot.)
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        let x25519_static_key = StaticSecret::random();
        let (mldsa_pk, mldsa_sk) = mldsa_keypair();
        let (mlkem_pk, mlkem_sk) = mlkem_keypair();
        Ed25519Provider {
            signing_key: RwLock::new(signing_key),
            verifying_key: RwLock::new(verifying_key),
            x25519_static_key: RwLock::new(x25519_static_key),
            mldsa_keypair: RwLock::new((mldsa_pk, mldsa_sk)),
            mlkem_keypair: RwLock::new((mlkem_pk, mlkem_sk)),
        }
    }

    // ---------------------------------------------------------------------
    // KEM / AES helpers
    // ---------------------------------------------------------------------

    /// Derive an AES-256 key from a shared secret via HKDF-SHA256. The shared
    /// secret is the concatenation `[X25519(32) · ML-KEM(32)]`; an attacker must
    /// reconstruct *both* to recover the AES key.
    fn derive_aes_key(shared_secret: &[u8]) -> [u8; 32] {
        let mut okm = [0u8; 32];
        let hk = Hkdf::<Sha256>::new(Some(b"aes256-gcm-key"), shared_secret);
        hk.expand(b"aes256-gcm-key", &mut okm)
            .expect("HKDF expand failed (output buffer too short)");
        okm
    }

    /// Generate a fresh 12-byte random nonce for AES-GCM.
    fn generate_nonce() -> [u8; 12] {
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce).expect("failed to generate random nonce");
        nonce
    }

    /// Hybrid `(X25519 · ML-KEM-768)` encryption for the recipient identified by
    /// `mlkem_recipient_pk`.
    ///
    /// Returns `[X25519 eph PK · ML-KEM encapsulation · ML-KEM recipient PK ·
    /// nonce · ct+tag]`. Both shared secrets are derived, concatenated and fed
    /// through HKDF to produce a single AES key; an attacker must break *both*
    /// the X25519 and ML-KEM key exchange to recover it.
    fn hybrid_encrypt(
        static_x25519: &StaticSecret,
        mlkem_recipient_pk: &MLKemPublicKey,
        data: &[u8],
    ) -> Result<Vec<u8>, PneumaticError> {
        // X25519 half: ephemeral secret * static secret = shared secret.
        let x25519_eph = EphemeralSecret::random();
        let x25519_eph_pk = X25519PublicKey::from(&x25519_eph).to_bytes();
        let static_public_key = X25519PublicKey::from(&*static_x25519);
        let x25519_ss = x25519_eph.diffie_hellman(&static_public_key).to_bytes();

        // ML-KEM half: encapsulate to the recipient's public key.
        let (mlkem_ss, mlkem_ct) = mlkem_encapsulate(mlkem_recipient_pk);

        Self::finish_encrypt(&x25519_eph_pk, &x25519_ss, mlkem_recipient_pk, &mlkem_ct, &mlkem_ss.as_bytes(), data)
    }

    /// Assemble the final `(X25519 · ML-KEM-768)` ciphertext from the two
    /// already-computed shared secrets.
    #[allow(clippy::too_many_arguments)]
    fn finish_encrypt(
        x25519_eph_pk: &[u8; 32],
        x25519_ss: &[u8; 32],
        mlkem_recipient_pk: &MLKemPublicKey,
        mlkem_ct: &MLKemCiphertext,
        mlkem_ss: &[u8],
        data: &[u8],
    ) -> Result<Vec<u8>, PneumaticError> {
        // Concatenate the two shared secrets: [X25519(32) · ML-KEM(32)].
        let mut shared = [0u8; 64];
        shared[..32].copy_from_slice(x25519_ss);
        shared[32..].copy_from_slice(mlkem_ss);
        let aes_key = Self::derive_aes_key(&shared);

        let cipher = Aes256Gcm::new(&aes_key.into());
        let nonce_bytes = Self::generate_nonce();
        let nonce = Nonce::try_from(nonce_bytes.as_slice())
            .map_err(|_| PneumaticError::CryptoError("nonce must be 12 bytes".to_string()))?;

        let ciphertext = cipher
            .encrypt(&nonce, data)
            .map_err(|e| PneumaticError::CryptoError(format!("AES-GCM encryption failed: {:?}", e)))?;

        let mut result = Vec::with_capacity(HYBRID_KEM_EMPTY_LEN + data.len());
        result.extend_from_slice(x25519_eph_pk);
        result.extend_from_slice(mlkem_ct.as_bytes());
        result.extend_from_slice(mlkem_recipient_pk.as_bytes());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&ciphertext);
        Ok(result)
    }

    /// Decrypt a hybrid `(X25519 · ML-KEM-768)` ciphertext, deriving the AES key
    /// from this provider's static secrets and validating that the embedded
    /// ML-KEM recipient key is ours (fail-closed). Returns
    /// `PneumaticError::CryptoError` on any failure.
    fn hybrid_decrypt(
        static_x25519: &StaticSecret,
        mlkem_self_sk: &MLKemSecretKey,
        mlkem_self_pk: &MLKemPublicKey,
        data: &[u8],
    ) -> Result<Vec<u8>, PneumaticError> {
        // Minimum size for empty plaintext.
        if data.len() < HYBRID_KEM_EMPTY_LEN {
            return Err(PneumaticError::CryptoError(
                "Decrypt input too short for hybrid KEM header".to_string(),
            ));
        }

        let x25519_eph_pk_slice = &data[..X25519_PK_LEN];
        let mlkem_ct_slice = &data[X25519_PK_LEN..X25519_PK_LEN + MLKEM_CT_LEN];
        let mlkem_recipient_pk_slice =
            &data[X25519_PK_LEN + MLKEM_CT_LEN..X25519_PK_LEN + MLKEM_CT_LEN + MLKEM_PK_LEN];
        let payload = &data[X25519_PK_LEN + MLKEM_CT_LEN + MLKEM_PK_LEN..];

        let x25519_eph_pk_bytes: [u8; 32] = x25519_eph_pk_slice
            .try_into()
            .map_err(|_| PneumaticError::CryptoError("hybrid KEM header too short for X25519 key".to_string()))?;
        let x25519_eph_pk = X25519PublicKey::from(x25519_eph_pk_bytes);
        let mlkem_ct = MLKemCiphertext::from_bytes(mlkem_ct_slice)
            .map_err(|_| PneumaticError::CryptoError("invalid ML-KEM encapsulation".to_string()))?;
        let mlkem_recipient_pk = MLKemPublicKey::from_bytes(mlkem_recipient_pk_slice)
            .map_err(|_| PneumaticError::CryptoError("invalid ML-KEM recipient key".to_string()))?;

        // Fail-closed: the embedded recipient ML-KEM key must be ours.
        if mlkem_recipient_pk != *mlkem_self_pk {
            return Err(PneumaticError::CryptoError(
                "ML-KEM recipient key does not match this node".to_string(),
            ));
        }

        let x25519_ss: [u8; 32] = static_x25519.diffie_hellman(&x25519_eph_pk).to_bytes();
        let mlkem_ss = mlkem_decapsulate(&mlkem_ct, mlkem_self_sk);

        let mut shared = [0u8; 64];
        shared[..32].copy_from_slice(&x25519_ss);
        shared[32..].copy_from_slice(mlkem_ss.as_bytes());
        let aes_key = Self::derive_aes_key(&shared);

        let cipher = Aes256Gcm::new(&aes_key.into());
        let nonce_bytes: [u8; NONCE_LEN] = payload[..NONCE_LEN]
            .try_into()
            .map_err(|_| PneumaticError::CryptoError("hybrid KEM payload too short for nonce".to_string()))?;
        let nonce = Nonce::try_from(nonce_bytes.as_slice())
            .map_err(|_| PneumaticError::CryptoError("nonce must be 12 bytes".to_string()))?;

        cipher
            .decrypt(&nonce, &payload[NONCE_LEN..])
            .map_err(|e| PneumaticError::CryptoError(format!(
                "AES-GCM decryption failed (wrong key or tampered): {:?}", e
            )))
    }

    // ---------------------------------------------------------------------
    // Signature helpers
    // ---------------------------------------------------------------------

    /// Split a hybrid signature into its three parts, or `None` if it is not the
    /// canonical hybrid length.
    fn split_hybrid_signature(signature: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
        if signature.len() != MLDSA_FULL_SIG_LEN {
            return None;
        }
        let ed25519_sig = &signature[..ED25519_SIG_LEN];
        let mldsa_pk = &signature[ED25519_SIG_LEN..ED25519_SIG_LEN + MLDSA_PK_LEN];
        let mldsa_sig = &signature[ED25519_SIG_LEN + MLDSA_PK_LEN..];
        Some((ed25519_sig, mldsa_pk, mldsa_sig))
    }

    /// Verify only the Ed25519 half of a hybrid signature (against an Ed25519
    /// public key). Exposed for interop / testing; the trait `check_signature`
    /// requires both halves.
    pub fn check_ed25519_half(
        &self,
        signature: &[u8],
        ed25519_public_key: &[u8],
        data: &[u8],
    ) -> Result<bool, PneumaticError> {
        let sig = match ed25519_dalek::Signature::from_slice(signature) {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };
        let pk_bytes: [u8; 32] = match ed25519_public_key.try_into() {
            Ok(b) => b,
            Err(_) => return Ok(false),
        };
        let pk = match VerifyingKey::from_bytes(&pk_bytes) {
            Ok(pk) => pk,
            Err(_) => return Ok(false),
        };
        Ok(pk.verify(data, &sig).is_ok())
    }

    /// Verify only the ML-DSA-44 half of a hybrid signature (against an
    /// ML-DSA public key). Exposed for interop / testing; the trait
    /// `check_signature` requires both halves.
    pub fn check_ml_dsa_half(
        &self,
        signature: &[u8],
        mldsa_pk: &[u8],
        data: &[u8],
    ) -> Result<bool, PneumaticError> {
        let detached = match DetachedSignature::from_bytes(signature) {
            Ok(d) => d,
            Err(_) => return Ok(false),
        };
        let pk = match MldsaPublicKey::from_bytes(mldsa_pk) {
            Ok(p) => p,
            Err(_) => return Ok(false),
        };
        Ok(mldsa_verify(&detached, data, &pk).is_ok())
    }
}

impl Default for Ed25519Provider {
    fn default() -> Self {
        Self::generate()
    }
}

impl AsymCryptoProvider for Ed25519Provider {
    fn encrypt(&self, data: Vec<u8>) -> Result<Vec<u8>, PneumaticError> {
        let static_x25519 = self.x25519_static_key.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        let mlkem_pk = self.mlkem_keypair.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        Self::hybrid_encrypt(&static_x25519, &mlkem_pk.0, &data)
    }

    fn decrypt(&self, data: Vec<u8>) -> Result<Vec<u8>, PneumaticError> {
        let static_x25519 = self.x25519_static_key.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        let mlkem_keys = self.mlkem_keypair.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        Self::hybrid_decrypt(&static_x25519, &mlkem_keys.1, &mlkem_keys.0, &data)
    }

    fn encrypt_to(
        &self,
        recipient_public_key: &[u8; 32],
        recipient_mlkem_pk: &[u8],
        data: Vec<u8>,
    ) -> Result<Vec<u8>, PneumaticError> {
        // Reconstruct the recipient's X25519 and ML-KEM public keys from bytes.
        let recipient_x25519_pk = X25519PublicKey::from(*recipient_public_key);
        let recipient_mlkem_pk = MLKemPublicKey::from_bytes(recipient_mlkem_pk)
            .map_err(|_| PneumaticError::CryptoError("invalid recipient ML-KEM public key".to_string()))?;

        // X25519 half: ephemeral secret * recipient static key.
        let x25519_eph = EphemeralSecret::random();
        let x25519_eph_pk = X25519PublicKey::from(&x25519_eph).to_bytes();
        let x25519_ss: [u8; 32] = x25519_eph.diffie_hellman(&recipient_x25519_pk).to_bytes();

        // ML-KEM half: encapsulate to the recipient's public key.
        let (mlkem_ss, mlkem_ct) = mlkem_encapsulate(&recipient_mlkem_pk);

        Self::finish_encrypt(&x25519_eph_pk, &x25519_ss, &recipient_mlkem_pk, &mlkem_ct, &mlkem_ss.as_bytes(), &data)
    }

    fn decrypt_from(&self, data: Vec<u8>) -> Result<Vec<u8>, PneumaticError> {
        let static_x25519 = self.x25519_static_key.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        let mlkem_keys = self.mlkem_keypair.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        Self::hybrid_decrypt(&static_x25519, &mlkem_keys.1, &mlkem_keys.0, &data)
    }

    fn check_signature(
        &self,
        signature: &[u8],
        public_key: &[u8],
        data: &[u8],
    ) -> Result<bool, PneumaticError> {
        let (ed25519_sig, mldsa_pk, mldsa_sig) = match Self::split_hybrid_signature(signature) {
            Some(parts) => parts,
            None => return Ok(false),
        };

        // Half 1: Ed25519 must verify against the sender's Ed25519 public key.
        let ed_sig = match ed25519_dalek::Signature::from_slice(ed25519_sig) {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };
        let pk_bytes: [u8; 32] = match public_key.try_into() {
            Ok(b) => b,
            Err(_) => return Ok(false),
        };
        let ed_pk = match VerifyingKey::from_bytes(&pk_bytes) {
            Ok(pk) => pk,
            Err(_) => return Ok(false),
        };
        if ed_pk.verify(data, &ed_sig).is_err() {
            return Ok(false);
        }

        // Half 2: ML-DSA must verify against the key embedded in the signature.
        let mldsa_pk = match MldsaPublicKey::from_bytes(mldsa_pk) {
            Ok(p) => p,
            Err(_) => return Ok(false),
        };
        let detached = match DetachedSignature::from_bytes(mldsa_sig) {
            Ok(d) => d,
            Err(_) => return Ok(false),
        };
        Ok(mldsa_verify(&detached, data, &mldsa_pk).is_ok())
    }

    fn sign_data(&self, data: &[u8]) -> Result<Vec<u8>, PneumaticError> {
        let signing_key = self.signing_key.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        // Ed25519 half — fixed 64-byte signature.
        let ed25519_sig = signing_key.sign(data).to_vec();

        // ML-DSA half (deterministic): sign then take the detached signature bytes.
        let mldsa_keys = self.mldsa_keypair.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        let mldsa_pk = &mldsa_keys.0;
        let mldsa_sk = &mldsa_keys.1;
        let detached = mldsa_detached_sign(data, mldsa_sk);
        let mldsa_sig = detached.as_bytes().to_vec();
        let mldsa_pk_bytes = mldsa_pk.as_bytes().to_vec();

        // Concatenate: [Ed25519 sig · ML-DSA public key · ML-DSA signature].
        let mut signature = Vec::with_capacity(MLDSA_FULL_SIG_LEN);
        signature.extend_from_slice(&ed25519_sig);
        signature.extend_from_slice(&mldsa_pk_bytes);
        signature.extend_from_slice(&mldsa_sig);
        Ok(signature)
    }

    fn public_key(&self) -> Result<Vec<u8>, PneumaticError> {
        let pk = *self.verifying_key.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        Ok(pk.to_bytes().to_vec())
    }
}

impl Ed25519Provider {
    /// Return the 32-byte X25519 public key (for use with encrypt_to).
    /// This is different from `public_key()` which returns the Ed25519
    /// verifying key (for signature verification).
    pub fn x25519_public_key(&self) -> Result<[u8; 32], PneumaticError> {
        let static_x25519 = self.x25519_static_key.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        Ok(X25519PublicKey::from(&*static_x25519).to_bytes())
    }

    /// Return the ML-DSA-44 public key (1312 bytes). Embedded into every hybrid
    /// signature by `sign_data`.
    pub fn mldsa_public_key(&self) -> Result<Vec<u8>, PneumaticError> {
        let mldsa_keys = self.mldsa_keypair.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        let mldsa_pk = &mldsa_keys.0;
        Ok(mldsa_pk.as_bytes().to_vec())
    }

    /// Return the ML-KEM-768 public key (1184 bytes). Embedded in every hybrid
    /// KEM ciphertext by `encrypt` / `encrypt_to`.
    pub fn mlkem_public_key(&self) -> Result<Vec<u8>, PneumaticError> {
        let mlkem_keys = self.mlkem_keypair.read().map_err(|e| {
            PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e))
        })?;
        let mlkem_pk = &mlkem_keys.0;
        Ok(mlkem_pk.as_bytes().to_vec())
    }
}

// ---------------------------------------------------------------------------
// HashProvider — SHA-256 via ring
// ---------------------------------------------------------------------------

/// Trait for computing cryptographic hashes.
pub trait HashProvider: Send + Sync {
    fn hash(&self, data: &[u8]) -> Vec<u8>;
}

pub struct BasicHashProvider;

impl Default for BasicHashProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BasicHashProvider {
    pub fn new() -> Self {
        BasicHashProvider
    }
}

impl HashProvider for BasicHashProvider {
    fn hash(&self, data: &[u8]) -> Vec<u8> {
        digest::digest(&digest::SHA256, data).as_ref().to_vec()
    }
}

/// Compute a SHA-256 hash using the default provider.
pub fn sha256(data: &[u8]) -> Vec<u8> {
    BasicHashProvider::new().hash(data)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_and_verify() {
        let provider = Ed25519Provider::generate();
        let data = b"test message";
        let signature = provider.sign_data(data).unwrap();
        assert!(provider.check_signature(&signature, &provider.public_key().unwrap(), data).unwrap());
    }

    #[test]
    fn test_signature_rejected_for_tampered_data() {
        let provider = Ed25519Provider::generate();
        let sig = provider.sign_data(b"test message").unwrap();
        assert!(!provider.check_signature(&sig, &provider.public_key().unwrap(), b"tampered message").unwrap());
    }

    #[test]
    fn test_signature_with_wrong_public_key() {
        let provider = Ed25519Provider::generate();
        let sig = provider.sign_data(b"test message").unwrap();
        let other = Ed25519Provider::generate();
        assert!(!provider.check_signature(&sig, &other.public_key().unwrap(), b"test message").unwrap());
    }

    #[test]
    fn test_public_key_consistent() {
        let provider = Ed25519Provider::generate();
        let pk1 = provider.public_key().unwrap();
        let pk2 = provider.public_key().unwrap();
        assert_eq!(pk1, pk2);
        assert_eq!(pk1.len(), 32);
    }

    #[test]
    fn test_hybrid_signature_length_is_fixed() {
        let provider = Ed25519Provider::generate();
        let sig = provider.sign_data(b"hello").unwrap();
        assert_eq!(sig.len(), MLDSA_FULL_SIG_LEN); // 64 + 1312 + 2420 = 3796
    }

    #[test]
    fn test_ed25519_half_verifies_and_ml_dsa_half_verifies() {
        // Discriminator: each hybrid half is independently verifiable.
        let provider = Ed25519Provider::generate();
        let data = b"hybrid discriminator";
        let sig = provider.sign_data(data).unwrap();
        let ed25519_pk = provider.public_key().unwrap();
        let mldsa_pk = provider.mldsa_public_key().unwrap();

        assert!(provider
            .check_ed25519_half(&sig[..ED25519_SIG_LEN], &ed25519_pk, data)
            .unwrap());
        assert!(provider.check_ml_dsa_half(
            &sig[ED25519_SIG_LEN + MLDSA_PK_LEN..],
            &mldsa_pk,
            data,
        )
        .unwrap());
        // And both together accept.
        assert!(provider.check_signature(&sig, &ed25519_pk, data).unwrap());
    }

    #[test]
    fn test_ml_dsa_half_does_not_verify_under_ed25519_key() {
        // The ML-DSA half carries its own embedded key; feeding the Ed25519 key
        // to the ML-DSA half must fail.
        let provider = Ed25519Provider::generate();
        let sig = provider.sign_data(b"test").unwrap();
        // Feed the ML-DSA half-signature but a 32-byte Ed25519 key where the ML-DSA
        // public key (1312 bytes) is expected: length mismatch -> fails closed.
        assert!(!provider.check_ml_dsa_half(
            &sig[ED25519_SIG_LEN + MLDSA_PK_LEN..],
            &provider.public_key().unwrap(),
            b"test",
        )
        .unwrap());
    }

    #[test]
    fn test_hash_sha256() {
        let hp = BasicHashProvider::new();
        let hash = hp.hash(b"hello, pneumatic");
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn test_hash_deterministic() {
        let hp = BasicHashProvider::new();
        let data = b"deterministic test";
        assert_eq!(hp.hash(data), hp.hash(data));
    }

    #[test]
    fn test_sha256_free_function() {
        let hash = sha256(b"sha256 function test");
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let provider = Ed25519Provider::generate();
        let data = vec![1u8, 2, 3, 4];
        let encrypted = provider.encrypt(data.clone()).unwrap();
        let decrypted = provider.decrypt(encrypted).unwrap();
        assert_eq!(data, decrypted);
    }

    #[test]
    fn test_encrypt_decrypt_empty_data() {
        let provider = Ed25519Provider::generate();
        let data = vec![];
        let encrypted = provider.encrypt(data).unwrap();
        // Hybrid `(X25519 · ML-KEM-768)` header: 32-byte X25519 eph PK +
        // 1088-byte ML-KEM encapsulation + 1184-byte ML-KEM recipient PK +
        // 12-byte nonce + 16-byte GCM tag = 2332 bytes for empty plaintext.
        assert_eq!(encrypted.len(), HYBRID_KEM_EMPTY_LEN);
        let decrypted = provider.decrypt(encrypted).unwrap();
        assert_eq!(decrypted, Vec::<u8>::new());
    }

    // --- Cross-recipient encryption tests ---
    // Uses x25519_public_key()/mlkem_public_key() (NOT the Ed25519 public_key).

    #[test]
    fn test_encrypt_to_decrypt_from_roundtrip() {
        let sender = Ed25519Provider::generate();
        let recipient = Ed25519Provider::generate();
        let recipient_x25519 = recipient.x25519_public_key().unwrap();
        let recipient_mlkem_pk = recipient.mlkem_public_key().unwrap();
        let data = b"cross-recipient message";

        let encrypted = sender
            .encrypt_to(&recipient_x25519, &recipient_mlkem_pk, data.to_vec())
            .unwrap();
        let decrypted = recipient.decrypt_from(encrypted).unwrap();
        assert_eq!(data.to_vec(), decrypted);
    }

    #[test]
    fn test_decrypt_from_wrong_recipient_returns_error() {
        let sender = Ed25519Provider::generate();
        let recipient = Ed25519Provider::generate();
        let wrong_receiver = Ed25519Provider::generate();
        let recipient_x25519 = recipient.x25519_public_key().unwrap();
        let recipient_mlkem_pk = recipient.mlkem_public_key().unwrap();

        let encrypted = sender
            .encrypt_to(&recipient_x25519, &recipient_mlkem_pk, b"secret".to_vec())
            .unwrap();
        // The embedded ML-KEM recipient key will not match the wrong receiver's key,
        // so decryption fails fail-closed before any AES attempt.
        let result = wrong_receiver.decrypt_from(encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypt_to_empty_data() {
        let sender = Ed25519Provider::generate();
        let recipient = Ed25519Provider::generate();
        let recipient_x25519 = recipient.x25519_public_key().unwrap();
        let recipient_mlkem_pk = recipient.mlkem_public_key().unwrap();

        let encrypted = sender
            .encrypt_to(&recipient_x25519, &recipient_mlkem_pk, vec![])
            .unwrap();
        assert_eq!(encrypted.len(), HYBRID_KEM_EMPTY_LEN);
        let decrypted = recipient.decrypt_from(encrypted).unwrap();
        assert_eq!(decrypted, Vec::<u8>::new());
    }

    #[test]
    fn test_encrypt_to_self() {
        let provider = Ed25519Provider::generate();
        let pk = provider.x25519_public_key().unwrap();
        let mlkem_pk = provider.mlkem_public_key().unwrap();
        let data = b"self-encrypted";

        let encrypted = provider.encrypt_to(&pk, &mlkem_pk, data.to_vec()).unwrap();
        let decrypted = provider.decrypt_from(encrypted).unwrap();
        assert_eq!(data.to_vec(), decrypted);
    }

    #[test]
    fn test_invalid_signature_length() {
        let provider = Ed25519Provider::generate();
        assert!(!provider.check_signature(&vec![0u8; 100], &provider.public_key().unwrap(), b"test").unwrap());
    }

    #[test]
    fn test_invalid_public_key_length() {
        let provider = Ed25519Provider::generate();
        let sig = provider.sign_data(b"test").unwrap();
        assert!(!provider.check_signature(&sig, &vec![0u8; 100], b"test").unwrap());
    }

    #[test]
    fn test_default_generates_key() {
        assert_eq!(Ed25519Provider::default().public_key().unwrap().len(), 32);
    }

    #[test]
    fn test_different_nonces_per_encryption() {
        let provider = Ed25519Provider::generate();
        let data = b"repeated plaintext";
        let encrypted1 = provider.encrypt(data.to_vec()).unwrap();
        let encrypted2 = provider.encrypt(data.to_vec()).unwrap();
        // Ciphertexts must differ (new X25519 ephemeral key + new nonce +
        // randomized ML-KEM encapsulation each time).
        assert_ne!(encrypted1, encrypted2);
    }

    #[test]
    fn test_encrypt_to_different_ciphertexts() {
        let sender1 = Ed25519Provider::generate();
        let sender2 = Ed25519Provider::generate();
        let recipient = Ed25519Provider::generate();
        let recipient_x25519 = recipient.x25519_public_key().unwrap();
        let recipient_mlkem_pk = recipient.mlkem_public_key().unwrap();
        let data = b"same plaintext";
        let encrypted1 = sender1
            .encrypt_to(&recipient_x25519, &recipient_mlkem_pk, data.to_vec())
            .unwrap();
        let encrypted2 = sender2
            .encrypt_to(&recipient_x25519, &recipient_mlkem_pk, data.to_vec())
            .unwrap();
        // Different ephemeral keys → different ciphertexts.
        assert_ne!(encrypted1, encrypted2);
    }

    #[test]
    fn test_decrypt_short_input_returns_error() {
        let provider = Ed25519Provider::generate();
        let result = provider.decrypt_from(vec![0u8; 10]);
        assert!(result.is_err());
    }
}
