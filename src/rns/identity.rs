//! Node identity: a persisted RNS transport keypair bound to an Ed25519
//! on-chain keypair by an Ed25519 "binding signature" over
//! `(rhash, requested_type, requester_types)`.
//!
//! The two keypairs are intentionally independent — the rhash cannot be
//! computed from the on-chain public key (correlation resistance), and each
//! can be rotated without the other. Both live in one keystore file
//! (`node_identity.json` by default), written with mode 0600.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rns_crypto::identity::Identity;
use rns_crypto::OsRng;
use serde::{Deserialize, Serialize};

use crate::crypto::{sha256, AsymCryptoProvider, Ed25519Provider};
use crate::encoding::serialize_to_bytes_rmp;
use crate::errors::PneumaticError;
use crate::node::NodeRegistryType;

/// Monotonic counter that disambiguates temp files created within one process
/// (see `unique_temp_path`).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Name a temp file for an atomic keystore write: `stem.<pid>.<counter>.tmp`.
///
/// It is placed inside the target's *own* directory so the final `rename`
/// stays on one filesystem and is atomic; a bare relative name would resolve
/// against the CWD and fail with `EXDEV`.
fn unique_temp_path(parent: &Path, stem: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("{}.{}-{}.tmp", stem, std::process::id(), n))
}

/// Path of the backup a keystore write produces next to `path`
/// (`node_identity.json` -> `node_identity.json.bak`).
fn backup_path_for(path: &Path) -> Option<PathBuf> {
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned())?;
    let parent = path.parent()?;
    Some(parent.join(format!("{}.bak", name)))
}

/// Recovery guidance appended to a corrupt-keystore error.
///
/// Names the `.bak` file when a backup exists and tells the operator to restore
/// it; otherwise (first boot, or corruption with no prior backup) it instructs
/// them to recover from a trusted source and explicitly warns *not* to
/// regenerate, since regenerating would orphan the node's on-chain stake.
fn keystore_recovery_hint(path: &Path) -> String {
    match (path.file_name(), path.parent()) {
        (Some(name), Some(parent)) => {
            let name = name.to_string_lossy().into_owned();
            let bak = parent.join(format!("{}.bak", name));
            if bak.exists() {
                format!(
                    "A previous keystore backup may exist at {} — restore it with `cp {} {}` and restart, or re-import the keys from a trusted source.",
                    bak.display(), bak.display(), path.display()
                )
            } else {
                "No backup is available to restore. Do not regenerate on a running node — that orphans the node's on-chain stake. Recover the keys from a secure offline copy or a trusted peer, or regenerate only on a fresh/unstaked node.".to_string()
            }
        }
        _ => "No backup path could be derived for this keystore. Recover the keys from a secure offline copy or a trusted peer; do not regenerate on a running node.".to_string(),
    }
}

/// Best-effort 0600 on a path (unix only); never aborts a write if the chmod
/// fails. Used to lock down the `.bak` file, which also holds private keys.
#[cfg(unix)]
fn chmod_0600_best_effort(path: &Path) {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn chmod_0600_best_effort(_path: &Path) {}

/// RAII guard for the intermediate temp file of an atomic keystore write.
///
/// Lives in the target's directory and is mode 0600 on unix. Deleted on drop
/// unless [`TempFile::commit`] renamed it onto the final destination, so a
/// failed write (or a panic) leaves no stray temp file behind.
struct TempFile {
    path: Option<PathBuf>,
    file: Option<fs::File>,
}

impl TempFile {
    /// Open a fresh temp file at `path`.
    fn from_path(path: PathBuf) -> Result<Self, PneumaticError> {
        let file = open_temp_file(&path)?;
        Ok(Self { path: Some(path), file: Some(file) })
    }

    /// Write the full payload to the temp file.
    fn write(&mut self, bytes: &[u8]) -> Result<(), PneumaticError> {
        let file = self.file.as_mut().expect("temp file opened");
        use std::io::Write;
        file.write_all(bytes)
            .map_err(|e| PneumaticError::CryptoError(format!("identity temp file: {}", e)))?;
        Ok(())
    }

    /// Durably flush the temp file before it is renamed into place.
    fn sync(&self) -> Result<(), PneumaticError> {
        let file = self.file.as_ref().expect("temp file opened");
        file.sync_all()
            .map_err(|e| PneumaticError::CryptoError(format!("identity temp file: {}", e)))?;
        Ok(())
    }

    /// Atomically rename the temp file onto `dest`, then drop without removing
    /// it (the guard's path is cleared). Consumes the guard.
    fn commit(mut self, dest: &Path) -> Result<(), PneumaticError> {
        let tmp = self.path.take().expect("TempFile committed exactly once");
        match fs::rename(&tmp, dest) {
            Ok(()) => Ok(()),
            Err(e) => Err(PneumaticError::CryptoError(format!("identity file: {}", e))),
        }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // On any path that did not `commit`, remove the leftover temp file.
        if let Some(p) = self.path.take() {
            let _ = fs::remove_file(&p);
        }
    }
}

/// Open a fresh temp file with the platform-appropriate mode.
fn open_temp_file(path: &Path) -> Result<fs::File, PneumaticError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| PneumaticError::CryptoError(format!("identity temp file: {}", e)))
    }
    #[cfg(not(unix))]
    {
        fs::File::create(path)
            .map_err(|e| PneumaticError::CryptoError(format!("identity temp file: {}", e)))
    }
}

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

    /// Sign an arbitrary message with this node's Ed25519 key.
    pub fn sign_message(&self, message: &[u8]) -> Result<Vec<u8>, PneumaticError> {
        self.ed25519.sign_data(message)
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

        // Recovery guidance for the corruption below (backup restore vs. a
        // no-backup first boot). Computed only after a successful read: a
        // failed read is an IO/permission error, not keystore corruption.
        let hint = keystore_recovery_hint(path);

        let file: IdentityFile = serde_json::from_str(&raw).map_err(|e| {
            PneumaticError::CryptoError(format!(
                "corrupt identity file {} ({}); refusing to regenerate. {}",
                path.display(),
                e,
                hint
            ))
        })?;

        let rns_sk = hex::decode(&file.rns_private_key).map_err(|e| {
            PneumaticError::CryptoError(format!(
                "corrupt RNS private key in {}: {}; {}",
                path.display(),
                e,
                hint
            ))
        })?;
        let ed_seed = hex::decode(&file.ed25519_seed).map_err(|e| {
            PneumaticError::CryptoError(format!(
                "corrupt Ed25519 seed in {}: {}; {}",
                path.display(),
                e,
                hint
            ))
        })?;
        let rns_sk: [u8; 64] = rns_sk.try_into().map_err(|_| {
            PneumaticError::CryptoError(format!(
                "RNS private key in {} must be 64 bytes; {}",
                path.display(),
                hint
            ))
        })?;
        let ed_seed: [u8; 32] = ed_seed.try_into().map_err(|_| {
            PneumaticError::CryptoError(format!(
                "Ed25519 seed in {} must be 32 bytes; {}",
                path.display(),
                hint
            ))
        })?;

        let rns = Identity::from_private_key(&rns_sk);
        let rns_pub = rns
            .get_public_key()
            .ok_or_else(|| {
                PneumaticError::CryptoError(format!(
                    "identity file {} is internally inconsistent: no public key derived; {}",
                    path.display(),
                    hint
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

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| PneumaticError::CryptoError(format!("identity dir: {}", e)))?;
        }

        // Back up the current good keystore before replacing it, so an
        // interrupted or corrupted future write can be restored. A copy
        // failure is a hard error — fail closed rather than overwrite with no
        // recovery. Skipped on first boot, when there is no prior file.
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "node_identity".to_string());
        if path.exists() {
            let bak = backup_path_for(path)
                .ok_or_else(|| {
                    PneumaticError::CryptoError(format!(
                        "identity file {} has no recognizable name; cannot back up",
                        path.display()
                    ))
                })?;
            fs::copy(path, &bak)
                .map_err(|e| PneumaticError::CryptoError(format!("identity backup: {}", e)))?;
            chmod_0600_best_effort(&bak);
        }

        // Write to a temp file, fsync it, then atomically `rename` into place.
        // A reader therefore never observes a torn file, and a process killed
        // mid-write leaves the existing keystore intact. Any error after the
        // temp is created removes it via the guard.
        let tmp_path = unique_temp_path(parent, &stem);
        let mut tmp = TempFile::from_path(tmp_path)?;
        tmp.write(raw.as_bytes())?;
        tmp.sync()?;
        tmp.commit(path)?;
        Ok(())
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

    // --- Phase 4.6: atomic keystore write -----------------------------------
    //
    // `load_or_create` only calls `write_file` when the target is absent, so it
    // never exercises the overwrite / atomic / backup paths. These tests call
    // `NodeIdentity::write_file` *directly*, otherwise they would silently pass
    // against a build where overwrite was never implemented.

    /// A fresh, valid 64-byte RNS private key.
    fn fresh_rns_sk() -> [u8; 64] {
        NodeIdentity::generate_in_memory()
            .rns
            .get_private_key()
            .expect("rns private key")
    }

    /// A random 32-byte Ed25519 seed.
    fn fresh_ed_seed() -> [u8; 32] {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("ed25519 seed");
        seed
    }

    #[test]
    fn test_write_file_writes_backup_on_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node_identity.json");
        let bak = dir.path().join("node_identity.json.bak");

        let seed1 = fresh_ed_seed();
        let k1 = Ed25519Provider::from_seed(seed1).public_key().unwrap();
        let sk1 = fresh_rns_sk();
        let sk2 = fresh_rns_sk();
        let seed2 = fresh_ed_seed();

        // First write: no prior file, so no backup yet.
        NodeIdentity::write_file(&path, &sk1, &seed1).expect("create v1");
        assert!(!bak.exists(), "no backup on first write");

        // Overwrite: the prior good keystore is backed up before replacement.
        NodeIdentity::write_file(&path, &sk2, &seed2).expect("overwrite v2");
        assert!(bak.exists(), "backup created on overwrite");
        assert!(path.exists(), "primary present after overwrite");

        // Backup holds v1, primary holds v2.
        let backup_loaded = NodeIdentity::load(&bak).expect("load backup");
        assert_eq!(
            k1,
            backup_loaded.ed25519.public_key().unwrap(),
            "backup holds the prior good keystore"
        );
        let primary_loaded = NodeIdentity::load(&path).expect("load primary");
        assert_ne!(
            k1,
            primary_loaded.ed25519.public_key().unwrap(),
            "primary holds the new keystore"
        );
    }

    #[test]
    fn test_corrupt_keystore_error_names_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node_identity.json");
        let bak = dir.path().join("node_identity.json.bak");

        NodeIdentity::write_file(&path, &fresh_rns_sk(), &fresh_ed_seed()).expect("v1");
        NodeIdentity::write_file(&path, &fresh_rns_sk(), &fresh_ed_seed()).expect("v2 -> .bak");
        assert!(bak.exists());

        // Clobber the primary; boot must fail closed and name the backup.
        fs::write(&path, b"this is not json at all").expect("clobber");
        let err = match NodeIdentity::load_or_create(&path) {
            Err(e) => e,
            Ok(_) => panic!("corrupt keystore must be an error, not a regeneration"),
        };
        let msg = format!("{:?}", err);
        assert!(msg.contains(".bak"), "error must name the backup: {}", msg);
        assert!(msg.contains("restore"), "error must instruct restore: {}", msg);
        assert!(msg.contains("refusing to regenerate"), "error must refuse regen: {}", msg);

        // Restore from the backup, then boot succeeds and yields the old rhash.
        fs::copy(&bak, &path).expect("restore");
        let v1 = NodeIdentity::load(&bak).expect("v1 identity");
        let restored = NodeIdentity::load_or_create(&path).expect("boot after restore");
        assert_eq!(restored.rhash, v1.rhash);
    }

    #[test]
    fn test_corrupt_keystore_without_backup_refuses_regenerate() {
        // First-boot corrupt file: no `.bak`, so the guidance takes the
        // "no backup" branch and still refuses to regenerate.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node_identity.json");
        fs::write(&path, b"not json").expect("clobber");
        let err = match NodeIdentity::load_or_create(&path) {
            Err(e) => e,
            Ok(_) => panic!("corrupt keystore must be an error"),
        };
        let msg = format!("{:?}", err);
        assert!(msg.contains("refusing to regenerate"), "{}", msg);
        assert!(
            !msg.contains("cp "),
            "no restore command when no backup exists: {}",
            msg
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_write_file_forces_0600_on_existing_file() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node_identity.json");

        // Pre-create the keystore with looser perms.
        fs::write(&path, b"{}").expect("precreate");
        fs::set_permissions(&path, Permissions::from_mode(0o644)).expect("set 0644");

        NodeIdentity::write_file(&path, &fresh_rns_sk(), &fresh_ed_seed()).expect("overwrite");

        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "overwrite must force 0600 on a pre-existing file");
    }

    #[test]
    fn test_atomic_write_roundtrips_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node_identity.json");

        NodeIdentity::write_file(&path, &fresh_rns_sk(), &fresh_ed_seed()).expect("write");
        NodeIdentity::load_or_create(&path).expect("load");

        // A successful atomic write leaves exactly one identity file and no
        // stray temp files in the directory.
        let tmp_count = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(tmp_count, 0, "no leftover temp files after atomic write");
        assert_eq!(NodeIdentity::load_or_create(&path).expect("reload").rhash.len(), 16);
    }

    #[test]
    fn test_partial_intermediate_preserves_primary() {
        // Documents crash-safety (not a discriminator: the old in-place write
        // also ignores a stray temp). A process killed mid-write leaves a
        // partial temp that is never renamed onto the target; the primary
        // keystore, written atomically on its own write, is observed intact by
        // the next boot.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node_identity.json");

        let id1 = NodeIdentity::generate_in_memory();
        NodeIdentity::write_file(&path, &id1.rns.get_private_key().unwrap(), &fresh_ed_seed())
            .expect("v1");

        // Simulate an abandoned, partially written temp from a crashed write.
        let tmp = dir.path().join(format!("node_identity.{}-1.tmp", std::process::id()));
        fs::write(&tmp, b"partial").expect("seed temp");

        let loaded = NodeIdentity::load_or_create(&path).expect("load after crash");
        assert_eq!(loaded.rhash, id1.rhash, "primary intact after a mid-write crash");
    }
}
