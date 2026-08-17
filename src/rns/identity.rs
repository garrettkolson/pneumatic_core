//! Node identity: a persisted RNS transport keypair bound to an Ed25519
//! on-chain keypair by an Ed25519 "binding signature" over
//! `(rhash, requested_type, requester_types)`.
//!
//! The two keypairs are intentionally independent — the rhash cannot be
//! computed from the on-chain public key (correlation resistance), and each
//! can be rotated without the other. Both live in one keystore file
//! (`node_identity.json` by default), written with mode 0600.

use std::fs;
use std::path::Path;

use rns_crypto::identity::Identity;
use rns_crypto::OsRng;
use serde::{Deserialize, Serialize};

use crate::crypto::{sha256, AsymCryptoProvider, Ed25519Provider};
use crate::encoding::serialize_to_bytes_rmp;
use crate::errors::PneumaticError;
use crate::node::NodeRegistryType;

/// The 16-byte transport address (rhash) derived from a 64-byte RNS public
/// key: the first 16 bytes of SHA-256. This matches rns-crypto's
/// `Identity::hash()` (asserted in tests), and it lets us derive the rhash
/// of a bootstrap peer from just its configured public key.
pub fn rhash_from_public_key(public_key: &[u8; 64]) -> [u8; 16] {
    let digest = sha256(public_key);
    let mut rhash = [0u8; 16];
    rhash.copy_from_slice(&digest[..16]);
    rhash
}

/// The persisted node identity: transport (RNS) + on-chain (Ed25519).
pub struct NodeIdentity {
    /// RNS transport identity (64-byte keypair).
    pub rns: Identity,
    /// Ed25519 on-chain identity: message signing + stake-registry key.
    pub ed25519: Ed25519Provider,
    /// 16-byte transport address (rhash).
    pub rhash: [u8; 16],
}

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    rns_private_key: String,
    ed25519_seed: String,
}

impl NodeIdentity {
    /// Load the identity from `path`, generating and persisting one if the
    /// file does not exist.
    ///
    /// On first creation the operator-facing keys are logged: the rhash,
    /// the Ed25519 public key hex, and the 64-byte RNS public key hex. The
    /// operator records the RNS public key in peers' `bootstrap_peers`.
    pub fn load_or_create(path: &Path) -> Result<Self, PneumaticError> {
        if path.exists() {
            Self::load(path)
        } else {
            Self::create_and_persist(path)
        }
    }

    /// Generate a fresh in-memory identity (no file I/O) for tests.
    pub fn generate_in_memory() -> Self {
        let rns = Identity::new(&mut OsRng);
        let rns_pub = rns
            .get_public_key()
            .expect("freshly generated identity must have a public key");
        let mut ed_seed = [0u8; 32];
        getrandom::getrandom(&mut ed_seed)
            .expect("failed to generate Ed25519 seed");
        let ed25519 = Ed25519Provider::from_seed(ed_seed);
        NodeIdentity {
            rns,
            ed25519,
            rhash: rhash_from_public_key(&rns_pub),
        }
    }

    /// Sign the binding tuple `(rhash, requested_type, requester_types)`
    /// with this node's Ed25519 key. Peers verify it to learn that the
    /// on-chain key really is bound to the transport rhash.
    pub fn sign_binding(
        &self,
        rhash: &[u8; 16],
        requested_type: &NodeRegistryType,
        requester_types: &[NodeRegistryType],
    ) -> Result<Vec<u8>, PneumaticError> {
        let payload = binding_payload(rhash, requested_type, requester_types)?;
        self.ed25519.sign_data(&payload)
    }

    /// Verify a peer's binding signature against its Ed25519 public key.
    pub fn verify_binding(
        ed25519_public_key: &[u8],
        rhash: &[u8; 16],
        requested_type: &NodeRegistryType,
        requester_types: &[NodeRegistryType],
        signature: &[u8],
    ) -> bool {
        match binding_payload(rhash, requested_type, requester_types) {
            Ok(payload) => {
                // A throwaway provider: check_signature only uses the
                // supplied public key.
                Ed25519Provider::generate()
                    .check_signature(signature, ed25519_public_key, &payload)
                    .unwrap_or(false)
            }
            Err(_) => false,
        }
    }

    /// Load an existing keystore. A missing or corrupt file is a hard error
    /// — we NEVER silently regenerate, because a new identity would orphan
    /// any stake registered under the old one.
    fn load(path: &Path) -> Result<Self, PneumaticError> {
        let raw = fs::read_to_string(path).map_err(|e| {
            PneumaticError::CryptoError(format!(
                "failed to read identity file {}: {}",
                path.display(),
                e
            ))
        })?;

        let file: IdentityFile = serde_json::from_str(&raw).map_err(|e| {
            PneumaticError::CryptoError(format!(
                "corrupt identity file {} ({}); refusing to regenerate",
                path.display(),
                e
            ))
        })?;

        let rns_sk = hex::decode(&file.rns_private_key).map_err(|e| {
            PneumaticError::CryptoError(format!(
                "corrupt RNS private key in {}: {}",
                path.display(),
                e
            ))
        })?;
        let ed_seed = hex::decode(&file.ed25519_seed).map_err(|e| {
            PneumaticError::CryptoError(format!(
                "corrupt Ed25519 seed in {}: {}",
                path.display(),
                e
            ))
        })?;
        let rns_sk: [u8; 64] = rns_sk.try_into().map_err(|_| {
            PneumaticError::CryptoError(format!(
                "RNS private key in {} must be 64 bytes",
                path.display()
            ))
        })?;
        let ed_seed: [u8; 32] = ed_seed.try_into().map_err(|_| {
            PneumaticError::CryptoError(format!(
                "Ed25519 seed in {} must be 32 bytes",
                path.display()
            ))
        })?;

        let rns = Identity::from_private_key(&rns_sk);
        let rns_pub = rns
            .get_public_key()
            .ok_or_else(|| {
                PneumaticError::CryptoError(format!(
                    "identity file {} is internally inconsistent: no public key derived",
                    path.display()
                ))
            })?;
        let ed25519 = Ed25519Provider::from_seed(ed_seed);

        Ok(NodeIdentity {
            rns,
            ed25519,
            rhash: rhash_from_public_key(&rns_pub),
        })
    }

    /// Generate a fresh identity, persist it with mode 0600, and log the
    /// operator-facing keys.
    fn create_and_persist(path: &Path) -> Result<Self, PneumaticError> {
        let rns = Identity::new(&mut OsRng);
        let rns_sk = rns.get_private_key().ok_or_else(|| {
            PneumaticError::CryptoError("RNS key generation: no private key".to_string())
        })?;
        let rns_pub = rns.get_public_key().ok_or_else(|| {
            PneumaticError::CryptoError("RNS key generation: no public key".to_string())
        })?;

        let mut ed_seed = [0u8; 32];
        getrandom::getrandom(&mut ed_seed)
            .map_err(|e| PneumaticError::CryptoError(format!("Ed25519 seed: {:?}", e)))?;
        let ed25519 = Ed25519Provider::from_seed(ed_seed);
        let ed_pub = ed25519.public_key()?;

        Self::write_file(path, &rns_sk, &ed_seed)?;

        let rhash = rhash_from_public_key(&rns_pub);
        eprintln!("[pneumatic] New node identity created at {}", path.display());
        eprintln!("[pneumatic]   rhash:          {}", hex::encode(rhash));
        eprintln!("[pneumatic]   ed25519 public: {}", hex::encode(ed_pub));
        eprintln!("[pneumatic]   rns public:     {}", hex::encode(rns_pub));
        eprintln!("[pneumatic] Record the rns public key in peers' `bootstrap_peers` to link this node.");

        Ok(NodeIdentity {
            rns,
            ed25519,
            rhash,
        })
    }

    fn write_file(path: &Path, rns_sk: &[u8; 64], ed_seed: &[u8; 32]) -> Result<(), PneumaticError> {
        let file = IdentityFile {
            rns_private_key: hex::encode(rns_sk),
            ed25519_seed: hex::encode(ed_seed),
        };
        let raw =
            serde_json::to_string_pretty(&file).map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| PneumaticError::CryptoError(format!("identity dir: {}", e)))?;
            }
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut out = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| PneumaticError::CryptoError(format!("identity file: {}", e)))?;
            use std::io::Write;
            out.write_all(raw.as_bytes())
                .map_err(|e| PneumaticError::CryptoError(format!("identity file: {}", e)))?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            fs::write(path, raw)
                .map_err(|e| PneumaticError::CryptoError(format!("identity file: {}", e)))
        }
    }
}

/// The exact bytes the binding signature covers. Both the signing and
/// verifying sides go through here so they can never drift.
fn binding_payload(
    rhash: &[u8; 16],
    requested_type: &NodeRegistryType,
    requester_types: &[NodeRegistryType],
) -> Result<Vec<u8>, PneumaticError> {
    serialize_to_bytes_rmp(&(rhash, requested_type, requester_types))
        .map_err(|e| PneumaticError::Encoding(e.to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rhash_matches_rns_identity_hash() {
        // Our sha256-truncation derivation must equal rns-crypto's own
        // identity hash, or the transport address would not match what
        // RNS announces.
        let id = NodeIdentity::generate_in_memory();
        let pub64 = id.rns.get_public_key().unwrap();
        assert_eq!(id.rhash, *id.rns.hash());
        assert_eq!(id.rhash, rhash_from_public_key(&pub64));
    }

    #[test]
    fn test_binding_sign_verify() {
        let a = NodeIdentity::generate_in_memory();
        let b = NodeIdentity::generate_in_memory();
        let a_pub = a.ed25519.public_key().unwrap();
        let b_pub = b.ed25519.public_key().unwrap();

        let sig = a
            .sign_binding(&a.rhash, &NodeRegistryType::Committer, &[NodeRegistryType::Committer])
            .expect("sign");

        assert!(NodeIdentity::verify_binding(
            &a_pub,
            &a.rhash,
            &NodeRegistryType::Committer,
            &[NodeRegistryType::Committer],
            &sig
        ));

        // Wrong key must fail.
        assert!(!NodeIdentity::verify_binding(
            &b_pub,
            &a.rhash,
            &NodeRegistryType::Committer,
            &[NodeRegistryType::Committer],
            &sig
        ));
    }

    #[test]
    fn test_binding_tampered_fields_rejected() {
        let a = NodeIdentity::generate_in_memory();
        let a_pub = a.ed25519.public_key().unwrap();
        let sig = a
            .sign_binding(&a.rhash, &NodeRegistryType::Committer, &[NodeRegistryType::Committer])
            .expect("sign");

        // Tamper the rhash.
        let mut bad_rhash = a.rhash;
        bad_rhash[0] ^= 0xff;
        assert!(!NodeIdentity::verify_binding(
            &a_pub,
            &bad_rhash,
            &NodeRegistryType::Committer,
            &[NodeRegistryType::Committer],
            &sig
        ));

        // Tamper the requested type.
        assert!(!NodeIdentity::verify_binding(
            &a_pub,
            &a.rhash,
            &NodeRegistryType::Executor,
            &[NodeRegistryType::Committer],
            &sig
        ));

        // Tamper the requester types.
        assert!(!NodeIdentity::verify_binding(
            &a_pub,
            &a.rhash,
            &NodeRegistryType::Committer,
            &[NodeRegistryType::Committer, NodeRegistryType::Sentinel],
            &sig
        ));
    }

    #[test]
    fn test_generate_in_memory_is_unique() {
        let a = NodeIdentity::generate_in_memory();
        let b = NodeIdentity::generate_in_memory();
        assert_ne!(a.rhash, b.rhash);
        assert_ne!(
            a.ed25519.public_key().unwrap(),
            b.ed25519.public_key().unwrap()
        );
    }

    #[test]
    fn test_keystore_persist_reload_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node_identity.json");

        let first = NodeIdentity::load_or_create(&path).expect("create");
        let first_ed = first.ed25519.public_key().unwrap();
        let first_rns = first.rns.get_public_key().unwrap();

        let second = NodeIdentity::load_or_create(&path).expect("reload");
        assert_eq!(first.rhash, second.rhash);
        assert_eq!(first_ed, second.ed25519.public_key().unwrap());
        assert_eq!(first_rns, second.rns.get_public_key().unwrap());
    }

    #[test]
    fn test_corrupt_keystore_is_hard_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node_identity.json");

        // Valid file first.
        NodeIdentity::load_or_create(&path).expect("create");

        // Clobber with garbage; loading must fail and NOT regenerate.
        fs::write(&path, b"this is not json at all").expect("clobber");
        // NodeIdentity is not Debug, so no `.expect_err()` — match instead.
        let err = match NodeIdentity::load_or_create(&path) {
            Err(e) => e,
            Ok(_) => panic!("corrupt keystore must be an error, not a regenerated identity"),
        };
        assert!(
            matches!(err, PneumaticError::CryptoError(_)),
            "expected CryptoError, got {:?}",
            err
        );
        // File untouched — no silent regeneration.
        assert_eq!(fs::read(&path).expect("read"), b"this is not json at all");

        // Valid JSON, wrong key lengths — also a hard error.
        fs::write(&path, r#"{"rns_private_key":"ab","ed25519_seed":"cd"}"#).expect("clobber");
        assert!(NodeIdentity::load_or_create(&path).is_err());
        assert_eq!(
            fs::read(&path).expect("read"),
            r#"{"rns_private_key":"ab","ed25519_seed":"cd"}"#.as_bytes()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_keystore_file_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node_identity.json");

        NodeIdentity::load_or_create(&path).expect("create");
        let mode = fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
