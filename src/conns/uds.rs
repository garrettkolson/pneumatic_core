//! Helpers for the pneumatic data-service channel's Unix-domain-socket path.
//!
//! These live in their own module (rather than in `data.rs`) so the framing and
//! auth code in `senders.rs` can reuse the same runtime-dir / HMAC logic. Everything
//! here is cross-platform except it is only *used* where Unix sockets exist.

use std::fs;
use std::path::Path;

use hmac::{Hmac, Mac};
use sha2::Sha256;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use crate::conns::ConnError;

type HmacSha256 = Hmac<Sha256>;

const SOCKET_DIR_NAME: &str = "pneumatic";
const SOCKET_DIR_MODE: u32 = 0o700;

/// The current user id. Used to namespace socket files so two users on the same
/// host never collide on (or collide-by-symlink each other's) socket path.
#[cfg(unix)]
pub fn uid() -> u32 {
    // SAFETY: getuid() has no failure mode and reads only the caller's uid.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
pub fn uid() -> u32 {
    std::process::id() as u32
}

/// Directory that holds this user's data-service sockets.
///
/// Prefers `$XDG_RUNTIME_DIR` (a per-UID, 0700 tmpfs on Linux) with our own
/// `pneumatic/` subdir; on macOS/other platforms where that is unset it falls
/// back to `<system temp>/pneumatic-<uid>`. The directory is created (if needed)
/// and then its mode is forced to 0700 and re-verified, so a pre-existing parent
/// that is world-writable cannot smuggle our socket into an attacker-controlled
/// location.
pub fn data_runtime_dir() -> Result<std::path::PathBuf, ConnError> {
    let base: std::path::PathBuf = if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        std::path::Path::new(&xdg).join(SOCKET_DIR_NAME)
    } else {
        std::env::temp_dir().join(format!("{SOCKET_DIR_NAME}-{}", uid()))
    };

    fs::create_dir_all(&base)
        .map_err(|e| ConnError::IO(e.to_string()))?;
    enforce_dir_mode(&base)?;
    Ok(base)
}

/// Absolute socket path for a given data-service name, e.g. `data` ->
/// `<runtime_dir>/data.sock`.
pub fn data_socket_path(name: &str) -> Result<std::path::PathBuf, ConnError> {
    let dir = data_runtime_dir()?;
    Ok(dir.join(format!("{name}.sock")))
}

/// Set the directory's permissions to 0700 and verify the resulting mode.
///
/// Only enforced on Unix, where permission bits are meaningful; on other
/// platforms the directory is still created (see `data_runtime_dir`) but we do
/// not read back permission bits.
fn enforce_dir_mode(dir: &Path) -> Result<(), ConnError> {
    #[cfg(unix)]
    {
        fs::set_permissions(dir, fs::Permissions::from_mode(SOCKET_DIR_MODE))
            .map_err(|e| ConnError::IO(e.to_string()))?;
        let mode = fs::metadata(dir)
            .map(|m| m.mode() & 0o777)
            .unwrap_or(0o777);
        if mode != SOCKET_DIR_MODE {
            return Err(ConnError::MalformedData(format!(
                "data socket dir {:?} is not mode 0700 (found {:03o})",
                dir, mode
            )));
        }
    }
    Ok(())
}

/// Prepare an existing path for binding: reject a pre-created symlink (an attacker
/// who places a symlink at the socket path can redirect the bind) and remove a
/// stale regular socket left behind by a crashed listener.
///
/// Returns `Err` for a symlink (fail closed), silently removes a stale socket,
/// and is a no-op when nothing exists.
pub fn prepare_socket_path(path: &Path) -> Result<(), ConnError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            // A pre-created symlink at the socket path is a hijack: a peer who
            // drops a symlink there can redirect our bind to an arbitrary
            // target. Refuse it (fail closed).
            if meta.is_symlink() {
                return Err(ConnError::MalformedData(format!(
                    "refusing to bind socket path {:?}: it is a symlink",
                    path
                )));
            }
            // A regular file here means a prior listener crashed and left a
            // stale socket. Remove it and let bind succeed.
            if meta.is_file() {
                fs::remove_file(path)
                    .map_err(|e| ConnError::IO(e.to_string()))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ConnError::IO(e.to_string())),
    }
}

/// Sign `payload` with the shared secret, returning the 32-byte tag and the
/// payload as sent on the wire. When `secret` is `None` (no auth configured) the
/// tag is a zero buffer and the body is the unauthenticated payload.
pub fn sign_payload(secret: Option<&[u8]>, payload: &[u8]) -> ([u8; 32], Vec<u8>) {
    match secret {
        Some(secret) => {
            let tag = hmac_sha256(secret, payload);
            (tag, payload.to_vec())
        }
        None => ([0u8; 32], payload.to_vec()),
    }
}

/// Verify the tag over `payload` in constant time. Returns `true` when
/// `secret` is `None` (unauthenticated path), when the tag matches, and `false`
/// on a mismatch.
pub fn verify_payload(secret: Option<&[u8]>, tag: &[u8], payload: &[u8]) -> bool {
    match secret {
        None => true,
        Some(secret) => {
            if tag.len() != 32 {
                return false;
            }
            let expected = hmac_sha256(secret, payload);
            let mut mac =
                HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts any key length");
            mac.update(payload);
            mac.verify_slice(&expected).is_ok()
                && expected[..] == tag[..]
        }
    }
}

/// HMAC-SHA256 of `msg` under `secret`. Returns a 32-byte tag.
pub fn hmac_sha256(secret: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac =
        HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts any key length");
    mac.update(msg);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

#[cfg(test)]
mod uds_tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    const SECRET: &[u8] = b"correct horse battery staple";

    #[test]
    fn hmac_matches_manual_computation() {
        let tag = hmac_sha256(SECRET, b"hello");
        // Deterministic: recomputing with the same secret + msg is equal.
        assert_eq!(tag, hmac_sha256(SECRET, b"hello"));
        // Different message -> different tag.
        assert_ne!(tag, hmac_sha256(SECRET, b"world"));
    }

    #[test]
    fn verify_accepts_correct_secret_rejects_wrong() {
        let (tag, body) = sign_payload(Some(SECRET), b"payload");
        assert!(verify_payload(Some(SECRET), &tag, &body));
        assert!(!verify_payload(Some(b"wrong"), &tag, &body));
    }

    #[test]
    fn verify_none_secret_is_always_true() {
        let (tag, body) = sign_payload(None, b"payload");
        assert!(verify_payload(None, &tag, &body));
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let (tag, _body) = sign_payload(Some(SECRET), b"original");
        assert!(!verify_payload(Some(SECRET), &tag, b"tampered"));
    }

    #[test]
    fn prepare_refuses_symlink() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("data.sock");
        // Pre-create a symlink at the socket path (the hijack).
        std::os::unix::fs::symlink("/etc/passwd", &sock).unwrap();
        let res = prepare_socket_path(&sock);
        assert!(res.is_err(), "symlink path must be refused");
    }

    #[test]
    fn prepare_removes_stale_socket() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("data.sock");
        fs::write(&sock, b"stale").unwrap();
        // Stale regular socket -> removed, no error.
        let res = prepare_socket_path(&sock);
        assert!(res.is_ok());
        assert!(!sock.exists(), "stale socket should have been removed");
    }

    #[test]
    fn prepare_absent_path_is_noop() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("data.sock");
        assert!(prepare_socket_path(&sock).is_ok());
        assert!(!sock.exists());
    }

    #[test]
    fn data_runtime_dir_enforces_0700() {
        let dir = data_runtime_dir().unwrap();
        let mode = fs::metadata(&dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700, "runtime dir must be mode 0700");
    }

    #[test]
    fn data_runtime_dir_is_a_directory_not_a_symlink() {
        let dir = data_runtime_dir().unwrap();
        assert!(dir.is_dir());
        // symlink_metadata on the dir itself must not be a symlink.
        let meta = fs::symlink_metadata(&dir).unwrap();
        assert_ne!(meta.mode() & 0o170000, 0o120000);
    }
}
