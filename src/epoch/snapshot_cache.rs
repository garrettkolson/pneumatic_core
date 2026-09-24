//! `EpochSnapshotCache<T>`: tiered epoch-snapshot cache (local map +
//! fetch hook) shared by the worker roles (S5.1 dedup of the three
//! worker-side copies).

use super::*;

// ---------------------------------------------------------------------------
// EpochSnapshotCache — tiered epoch-snapshot cache (local Map → fetch hook)
// ---------------------------------------------------------------------------

use std::sync::Mutex;
use crate::data::DataError;

/// Tiered epoch-snapshot cache shared by the worker roles.
///
/// Holds per-epoch values (e.g. [`StakeSet`] / [`ExecutorSet`]) in a local
/// `Mutex<HashMap<u64, T>>` tier (O(1) hits) with a **fetch hook** used as
/// the ~1 ms DataProvider fallback on a miss. A **peer tier** (requesting
/// the snapshot from a neighboring node's DataProvider) is reserved for
/// future use and not wired here.
///
/// This is the single implementation of what was previously three
/// near-identical worker-crate copies (`StakeSnapshotCache` in both the
/// sentinel and finalizer crates, and `ExecutorSetCache` in the sentinel
/// crate).
pub struct EpochSnapshotCache<T: Clone + Send> {
    /// Local tier: epoch → snapshot.
    local: Mutex<HashMap<u64, T>>,
    /// Tier-2 fallback: fetch the snapshot for an epoch from the local
    /// DataProvider (or other backing store). The closure captures whatever
    /// it needs (typically an `Arc<dyn DataProvider>`) and is invoked only
    /// on a local-tier miss.
    fetch: Box<dyn Fn(u64, &str) -> Result<T, DataError> + Send + Sync>,
    /// Partition id passed to the fetch hook.
    partition_id: String,
}

impl<T: Clone + Send> EpochSnapshotCache<T> {
    /// Create a new cache with no cached snapshots.
    pub fn new<F>(partition_id: String, fetch: F) -> Self
    where
        F: Fn(u64, &str) -> Result<T, DataError> + Send + Sync + 'static,
    {
        EpochSnapshotCache {
            local: Mutex::new(HashMap::new()),
            fetch: Box::new(fetch),
            partition_id,
        }
    }

    /// Get a snapshot for the given epoch.
    ///
    /// Tries the local cache first. If not found, invokes the fetch hook
    /// and caches the result for future lookups.
    pub fn get(&self, epoch: u64) -> Option<T> {
        // Tier 1: Local cache hit
        if let Some(snapshot) = self.local.lock().unwrap().get(&epoch).cloned() {
            return Some(snapshot);
        }

        // Tier 2: DataProvider (fetch hook) fallback
        match (self.fetch)(epoch, &self.partition_id) {
            Ok(snapshot) => {
                self.local.lock().unwrap().insert(epoch, snapshot.clone());
                Some(snapshot)
            }
            Err(e) => {
                log::warn!("DataProvider returned error for epoch {}: {:?}", epoch, e);
                None
            }
        }
    }

    /// Put a snapshot directly into the local cache.
    /// Called when a block from a new epoch is observed.
    pub fn put(&self, epoch: u64, snapshot: T) {
        self.local.lock().unwrap().insert(epoch, snapshot);
    }

    /// Returns the number of cached epochs.
    pub fn cached_count(&self) -> usize {
        self.local.lock().unwrap().len()
    }

    /// Invalidate all cached snapshots.
    ///
    /// Called on epoch transition to force a fresh fetch of the next epoch's
    /// snapshot.
    pub fn invalidate_all(&self) {
        self.local.lock().unwrap().clear();
    }
}
