use std::sync::{Arc, RwLock};
use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use ed25519_dalek::{SigningKey, Signer, Verifier, VerifyingKey};
use ring::digest;
use serde::{Deserialize, Serialize};
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};
use hkdf::Hkdf;
use sha2::Sha256;

// ---------------------------------------------------------------------------
// AsymCryptoProvider — Ed25519 for blockchain signing/verification
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub enum AsymCryptoProviderType {
    RSA,
}

pub fn get_asym_provider(provider_type: &AsymCryptoProviderType) -> Arc<RwLock<dyn AsymCryptoProvider>> {
    match provider_type {
        AsymCryptoProviderType::RSA => {
            let provider = Ed25519Provider::generate();
            Arc::new(RwLock::new(provider))
        }
    }
}

pub trait AsymCryptoProvider: Send + Sync {
    fn encrypt(&self, data: Vec<u8>) -> Vec<u8>;
    fn decrypt(&self, data: Vec<u8>) -> Vec<u8>;
    /// Encrypt `data` to an arbitrary recipient identified by their 32-byte
    /// X25519 public key.  Anyone with the recipient's public key can encrypt;
    /// only the recipient (holding the matching static secret) can decrypt.
    fn encrypt_to(&self, recipient_public_key: &[u8; 32], data: Vec<u8>) -> Vec<u8>;
    /// Decrypt data that was encrypted to this provider via `encrypt_to`.
    /// The sender's ephemeral public key is embedded in the ciphertext.
    fn decrypt_from(&self, data: Vec<u8>) -> Vec<u8>;
    fn check_signature(&self, signature: &[u8], public_key: &[u8], data: &[u8]) -> bool;
    fn sign_data(&self, data: &[u8]) -> Vec<u8>;
    fn public_key(&self) -> Vec<u8>;
}

/// Ed25519 asymmetric crypto provider backed by ed25519-dalek.
///
/// Ed25519 is used instead of RSA for blockchain signatures because:
/// - Fixed 32-byte keys (vs 2048+ bit RSA keys)
/// - Constant-time signature generation
/// - No padding oracle vulnerabilities
/// - Better performance per security level
///
/// Hybrid encryption uses AES-256-GCM with ephemeral X25519 key exchange:
/// each `encrypt()` call generates a new ephemeral keypair, derives a shared
/// secret via Diffie-Hellman, derives an AES key via HKDF-SHA256, and encrypts
/// the payload with a fresh random nonce. Output format:
/// `[32-byte ephemeral PK][12-byte nonce][ciphertext + 16-byte GCM tag]`
pub struct Ed25519Provider {
    signing_key: RwLock<SigningKey>,
    verifying_key: RwLock<VerifyingKey>,
    x25519_static_key: RwLock<StaticSecret>,
}

impl Ed25519Provider {
    /// Generate a fresh Ed25519 keypair with a random 32-byte seed,
    /// plus a separate X25519 static secret for key exchange.
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("Failed to generate random seed");
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        let x25519_static_key = StaticSecret::random();
        Ed25519Provider {
            signing_key: RwLock::new(signing_key),
            verifying_key: RwLock::new(verifying_key),
            x25519_static_key: RwLock::new(x25519_static_key),
        }
    }

    /// Derive an AES-256 key from X25519 shared secret via HKDF-SHA256.
    fn derive_aes_key(shared_secret: &[u8; 32]) -> [u8; 32] {
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

    /// Encrypt using a static secret (self or recipient) + ephemeral secret.
    /// Returns `[32-byte ephemeral PK][12-byte nonce][ciphertext + GCM tag]`.
    fn dh_encrypt(
        static_secret: &StaticSecret,
        data: &[u8],
    ) -> Vec<u8> {
        let ephemeral = EphemeralSecret::random();
        let ephemeral_pub = PublicKey::from(&ephemeral);
        let ephemeral_pub_bytes = ephemeral_pub.to_bytes();

        let shared_secret = static_secret.diffie_hellman(&ephemeral_pub);
        let aes_key = Self::derive_aes_key(shared_secret.as_bytes());
        let cipher = Aes256Gcm::new(&aes_key.into());
        let nonce_bytes = Self::generate_nonce();
        let nonce = Nonce::try_from(nonce_bytes.as_slice())
            .expect("nonce must be 12 bytes");

        let ciphertext = cipher
            .encrypt(&nonce, data)
            .expect("AES-GCM encryption failed");

        let mut result = Vec::with_capacity(32 + 12 + ciphertext.len());
        result.extend_from_slice(&ephemeral_pub_bytes);
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&ciphertext);
        result
    }

    /// Decrypt using a static secret + ephemeral public key from ciphertext.
    /// Expects `[32-byte ephemeral PK][12-byte nonce][ciphertext + GCM tag]`.
    fn dh_decrypt(static_secret: &StaticSecret, data: &[u8]) -> Vec<u8> {
        if data.len() < 44 {
            panic!("Decrypt input too short to contain ephemeral PK and nonce");
        }
        let ephemeral_pub_bytes: [u8; 32] = data[..32].try_into().map_err(|_| {
            panic!("Decrypt input too short to contain X25519 public key");
        })
        .expect("Invalid ephemeral public key");
        let ephemeral_pub = PublicKey::from(ephemeral_pub_bytes);

        let shared_secret = static_secret.diffie_hellman(&ephemeral_pub);
        let aes_key = Self::derive_aes_key(shared_secret.as_bytes());
        let cipher = Aes256Gcm::new(&aes_key.into());
        let nonce_bytes: [u8; 12] = data[32..44].try_into()
            .expect("ciphertext too short to contain nonce");
        let nonce = Nonce::try_from(nonce_bytes.as_slice())
            .expect("nonce must be 12 bytes");

        let plaintext = cipher
            .decrypt(&nonce, &data[44..])
            .expect("AES-GCM decryption failed (wrong key or tampered)");
        plaintext.to_vec()
    }
}

impl Default for Ed25519Provider {
    fn default() -> Self { Self::generate() }
}

impl AsymCryptoProvider for Ed25519Provider {
    fn encrypt(&self, data: Vec<u8>) -> Vec<u8> {
        let static_secret = self.x25519_static_key.read().expect("RwLock poisoned");
        Self::dh_encrypt(&static_secret, &data)
    }

    fn decrypt(&self, data: Vec<u8>) -> Vec<u8> {
        let static_secret = self.x25519_static_key.read().expect("RwLock poisoned");
        Self::dh_decrypt(&static_secret, &data)
    }

    fn encrypt_to(&self, recipient_public_key: &[u8; 32], data: Vec<u8>) -> Vec<u8> {
        let recipient_key = PublicKey::from(*recipient_public_key);

        // Generate fresh ephemeral keypair for this encryption.
        let ephemeral = EphemeralSecret::random();
        let ephemeral_pub = PublicKey::from(&ephemeral);
        let ephemeral_pub_bytes = ephemeral_pub.to_bytes();

        // DH: ephemeral secret * recipient's static public key = shared secret.
        // Anyone who knows the recipient's public key can encrypt to them.
        let shared_secret = ephemeral.diffie_hellman(&recipient_key);
        let aes_key = Self::derive_aes_key(shared_secret.as_bytes());
        let cipher = Aes256Gcm::new(&aes_key.into());
        let nonce_bytes = Self::generate_nonce();
        let nonce = Nonce::try_from(nonce_bytes.as_slice())
            .expect("nonce must be 12 bytes");

        let ciphertext = cipher
            .encrypt(&nonce, data.as_ref())
            .expect("AES-GCM encryption failed");

        let mut result = Vec::with_capacity(32 + 12 + ciphertext.len());
        result.extend_from_slice(&ephemeral_pub_bytes);
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&ciphertext);
        result
    }

    fn decrypt_from(&self, data: Vec<u8>) -> Vec<u8> {
        let static_secret = self.x25519_static_key.read().expect("RwLock poisoned");
        Self::dh_decrypt(&static_secret, &data)
    }

    fn check_signature(&self, signature: &[u8], public_key: &[u8], data: &[u8]) -> bool {
        let sig = match ed25519_dalek::Signature::from_slice(signature) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let pk_bytes: [u8; 32] = match public_key.try_into() {
            Ok(b) => b,
            Err(_) => return false,
        };
        let pk = match VerifyingKey::from_bytes(&pk_bytes) {
            Ok(pk) => pk,
            Err(_) => return false,
        };
        pk.verify(data, &sig).is_ok()
    }

    fn sign_data(&self, data: &[u8]) -> Vec<u8> {
        let signing_key = self.signing_key.read().expect("RwLock poisoned");
        let sig = signing_key.sign(data);
        sig.to_vec()
    }

    fn public_key(&self) -> Vec<u8> {
        let pk = *self.verifying_key.read().expect("RwLock poisoned");
        pk.to_bytes().to_vec()
    }
}

impl Ed25519Provider {
    /// Return the 32-byte X25519 public key (for use with encrypt_to).
    /// This is different from `public_key()` which returns the Ed25519
    /// verifying key (for signature verification).
    pub fn x25519_public_key(&self) -> [u8; 32] {
        let static_secret = self.x25519_static_key.read().expect("RwLock poisoned");
        PublicKey::from(&*static_secret).to_bytes()
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
    fn default() -> Self { Self::new() }
}

impl BasicHashProvider {
    pub fn new() -> Self { BasicHashProvider }
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
        let signature = provider.sign_data(data);
        assert!(provider.check_signature(&signature, &provider.public_key(), data));
    }

    #[test]
    fn test_signature_rejected_for_tampered_data() {
        let provider = Ed25519Provider::generate();
        let sig = provider.sign_data(b"test message");
        assert!(!provider.check_signature(&sig, &provider.public_key(), b"tampered message"));
    }

    #[test]
    fn test_signature_with_wrong_public_key() {
        let provider = Ed25519Provider::generate();
        let sig = provider.sign_data(b"test message");
        let other = Ed25519Provider::generate();
        assert!(!provider.check_signature(&sig, &other.public_key(), b"test message"));
    }

    #[test]
    fn test_public_key_consistent() {
        let provider = Ed25519Provider::generate();
        let pk1 = provider.public_key();
        let pk2 = provider.public_key();
        assert_eq!(pk1, pk2);
        assert_eq!(pk1.len(), 32);
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
        let encrypted = provider.encrypt(data.clone());
        let decrypted = provider.decrypt(encrypted);
        assert_eq!(data, decrypted);
    }

    #[test]
    fn test_encrypt_decrypt_empty_data() {
        let provider = Ed25519Provider::generate();
        let data = vec![];
        let encrypted = provider.encrypt(data);
        assert_eq!(encrypted.len(), 60); // 32-byte PK + 12-byte nonce + 16-byte GCM tag only
        let decrypted = provider.decrypt(encrypted);
        assert_eq!(decrypted, Vec::<u8>::new());
    }

    // --- Cross-recipient encryption tests (P6_05) ---
    // Note: uses x25519_public_key(), NOT public_key() (which returns Ed25519 verifying key)

    #[test]
    fn test_encrypt_to_decrypt_from_roundtrip() {
        let sender = Ed25519Provider::generate();
        let recipient = Ed25519Provider::generate();
        let recipient_pk = recipient.x25519_public_key();
        let data = b"cross-recipient message";

        let encrypted = sender.encrypt_to(&recipient_pk, data.to_vec());
        let decrypted = recipient.decrypt_from(encrypted);
        assert_eq!(data.to_vec(), decrypted);
    }

    #[test]
    fn test_encrypt_to_wrong_recipient_panics() {
        let sender = Ed25519Provider::generate();
        let recipient = Ed25519Provider::generate();
        let wrong_receiver = Ed25519Provider::generate();
        let recipient_pk = recipient.x25519_public_key();

        let encrypted = sender.encrypt_to(&recipient_pk, b"secret".to_vec());
        // Decrypting with a different provider's key should panic (AES-GCM tag mismatch).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wrong_receiver.decrypt_from(encrypted);
        }));
        assert!(result.is_err(), "decrypt_from should panic with wrong key");
    }

    #[test]
    fn test_encrypt_to_empty_data() {
        let sender = Ed25519Provider::generate();
        let recipient = Ed25519Provider::generate();
        let recipient_pk = recipient.x25519_public_key();

        let encrypted = sender.encrypt_to(&recipient_pk, vec![]);
        assert_eq!(encrypted.len(), 60); // 32-byte PK + 12-byte nonce + 16-byte GCM tag only
        let decrypted = recipient.decrypt_from(encrypted);
        assert_eq!(decrypted, Vec::<u8>::new());
    }

    #[test]
    fn test_encrypt_to_self() {
        let provider = Ed25519Provider::generate();
        let pk = provider.x25519_public_key();
        let data = b"self-encrypted";

        let encrypted = provider.encrypt_to(&pk, data.to_vec());
        let decrypted = provider.decrypt_from(encrypted);
        assert_eq!(data.to_vec(), decrypted);
    }

    #[test]
    fn test_invalid_signature_length() {
        let provider = Ed25519Provider::generate();
        assert!(!provider.check_signature(&vec![0u8; 100], &provider.public_key(), b"test"));
    }

    #[test]
    fn test_invalid_public_key_length() {
        let provider = Ed25519Provider::generate();
        let sig = provider.sign_data(b"test");
        assert!(!provider.check_signature(&sig, &vec![0u8; 100], b"test"));
    }

    #[test]
    fn test_default_generates_key() {
        assert_eq!(Ed25519Provider::default().public_key().len(), 32);
    }

    #[test]
    fn test_different_nonces_per_encryption() {
        let provider = Ed25519Provider::generate();
        let data = b"repeated plaintext";
        let encrypted1 = provider.encrypt(data.to_vec());
        let encrypted2 = provider.encrypt(data.to_vec());
        // Ciphertexts must differ (new ephemeral key + new nonce each time)
        assert_ne!(encrypted1, encrypted2);
    }

    #[test]
    fn test_encrypt_to_different_ciphertexts() {
        let sender1 = Ed25519Provider::generate();
        let sender2 = Ed25519Provider::generate();
        let recipient = Ed25519Provider::generate();
        let recipient_pk = recipient.x25519_public_key();
        let data = b"same plaintext";
        let encrypted1 = sender1.encrypt_to(&recipient_pk, data.to_vec());
        let encrypted2 = sender2.encrypt_to(&recipient_pk, data.to_vec());
        // Different ephemeral keys → different ciphertexts
        assert_ne!(encrypted1, encrypted2);
    }
}
