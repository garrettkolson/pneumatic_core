//! The key/value store behind the data service.
//!
//! # Why the store is byte-opaque
//!
//! Every persisted value that needs integrity protection travels inside its own
//! SHA-256 envelope (`StakeSnapshotEnvelope`, `ExecutorSetEnvelope`,
//! `ShieldedPoolStateEnvelope`), and the **client** re-verifies the fingerprint
//! on load (`src/data.rs`, `get_stake_snapshot` / `get_shielded_pool`). So this
//! store does not interpret values: it stores the exact bytes a `Save` carried
//! and returns them verbatim on a `Get`. Recomputing hashes here would be both
//! redundant and a drift risk — a service that "fixes up" a hash would defeat
//! the fail-closed contract the nodes depend on.
//!
//! Absence is deliberately *not* modelled as a value: the wire protocol has no
//! not-found signal (a `Get` response body is deserialized straight into the
//! requested type), so a miss returns an empty body and the client surfaces a
//! `DeserializationError`. That is the conservative direction — the alternative
//! is inventing a value the client would trust.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One stored record in the on-disk form (hex-encoded so the file is diffable).
#[derive(Serialize, Deserialize)]
struct PersistedEntry {
    partition_id: String,
    key: String,
    value: String,
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedStore {
    entries: Vec<PersistedEntry>,
}

/// Thread-safe `(partition_id, key) -> value` map with optional write-through
/// persistence.
///
/// A `Mutex<HashMap>` rather than a `DashMap` (ADR-002's DashMap choice is for
/// the hot, read-heavy *registry* state): the data service answers one request
/// per connection and is not on a consensus hot path, so per-shard locking
/// would buy nothing here. Lock poisoning is recovered via `into_inner` — the
/// map holds no invariants a panic could half-update in a way that matters for
/// a byte store, and refusing to serve after an unrelated panic would take the
/// whole cluster down with it.
pub struct DataStore {
    entries: Mutex<HashMap<(String, Vec<u8>), Vec<u8>>>,
    state_file: Option<PathBuf>,
}

impl Default for DataStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DataStore {
    /// An empty, non-persisting store (test / throwaway testnet use).
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            state_file: None,
        }
    }

    /// Load the store from `path` if it exists, and write through every
    /// subsequent `put`. Persisting matters for a testnet: chain state and the
    /// shielded pool live here, so nodes must be restartable without the pool
    /// authority forgetting prior spends.
    pub fn with_state_file(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let path = path.as_ref().to_path_buf();
        let entries = if path.exists() {
            let raw = fs::read(&path)?;
            let persisted: PersistedStore = serde_json::from_slice(&raw)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let mut map = HashMap::with_capacity(persisted.entries.len());
            for entry in persisted.entries {
                let key = hex::decode(&entry.key)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let value = hex::decode(&entry.value)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                map.insert((entry.partition_id, key), value);
            }
            map
        } else {
            HashMap::new()
        };
        Ok(Self {
            entries: Mutex::new(entries),
            state_file: Some(path),
        })
    }

    /// The value stored for `(partition_id, key)`, or `None` when absent.
    pub fn get(&self, partition_id: &str, key: &[u8]) -> Option<Vec<u8>> {
        let guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        guard.get(&(partition_id.to_string(), key.to_vec())).cloned()
    }

    /// Store `value` verbatim under `(partition_id, key)`, then (when a state
    /// file is configured) persist the whole store.
    pub fn put(&self, partition_id: &str, key: &[u8], value: &[u8]) -> Result<(), io::Error> {
        {
            let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            guard.insert((partition_id.to_string(), key.to_vec()), value.to_vec());
        }
        self.persist()
    }

    /// Number of stored records.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// True when nothing is stored. Required alongside [`DataStore::len`].
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `(partition_id, key)` pairs currently stored, sorted — for diagnostics
    /// and tests.
    pub fn keys(&self) -> Vec<(String, Vec<u8>)> {
        let mut keys: Vec<(String, Vec<u8>)> = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// Write the whole store to the configured state file (atomic
    /// temp+rename, so a crash mid-write cannot truncate a live testnet's
    /// chain state). A no-op when persistence is off.
    pub fn persist(&self) -> Result<(), io::Error> {
        let Some(path) = &self.state_file else {
            return Ok(());
        };
        let snapshot: Vec<PersistedEntry> = {
            let guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            let mut entries: Vec<PersistedEntry> = guard
                .iter()
                .map(|((partition_id, key), value)| PersistedEntry {
                    partition_id: partition_id.clone(),
                    key: hex::encode(key),
                    value: hex::encode(value),
                })
                .collect();
            // Deterministic file order so the state file is diffable across runs.
            entries.sort_by(|a, b| {
                (&a.partition_id, &a.key).cmp(&(&b.partition_id, &b.key))
            });
            entries
        };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let tmp = path.with_extension("tmp");
        let body = serde_json::to_vec_pretty(&PersistedStore { entries: snapshot })
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, &body)?;
        fs::rename(&tmp, path)
    }
}
