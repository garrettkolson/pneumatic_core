//! Stake snapshot cache for quorum gossip.
//!
//! Two-tier cache (mirrors the sentinel's implementation):
//! 1. **Local** (Mutex<HashMap>) — O(1), fetched once per epoch
//! 2. **DataProvider** — ~1ms, local service call
//!
//! The finalizer needs a stake snapshot for every `BlockFinalized` broadcast
//! so receiving nodes can perform stake-weighted confirmation tracking.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use pneumatic_core::data::DataProvider;
use pneumatic_core::epoch::StakeSet;

/// Local cache backed by a Mutex for lock-free concurrency.
struct LocalCache {
    /// epoch → stake snapshot
    cache: Mutex<HashMap<u64, StakeSet>>,
}

impl LocalCache {
    fn new() -> Self {
        LocalCache {
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, epoch: u64) -> Option<StakeSet> {
        self.cache.lock().get(&epoch).cloned()
    }

    fn put(&self, epoch: u64, snapshot: StakeSet) {
        self.cache.lock().insert(epoch, snapshot);
    }

    fn len(&self) -> usize {
        self.cache.lock().len()
    }

    fn invalidate_all(&self) {
        self.cache.lock().clear();
    }
}

/// Two-tier stake snapshot cache for the finalizer.
///
/// The finalizer includes the current epoch's stake set in
/// `BlockFinalized` gossip messages. This cache provides the snapshot with:
///
/// 1. **Local cache hit** (O(1)): Snapshot was fetched earlier this epoch.
///    This is the happy path — zero network latency.
///
/// 2. **DataProvider fallback (~1ms)**: If the local cache doesn't have it,
///    the finalizer calls the local DataProvider service via TCP/UDS.
///
/// Cache is invalidated on epoch transition to force a fresh fetch of the
/// new epoch's stake snapshot.
pub struct StakeSnapshotCache {
    local: LocalCache,
    data_provider: Arc<dyn DataProvider>,
    partition_id: String,
}

impl StakeSnapshotCache {
    /// Create a new cache with no cached snapshots.
    pub fn new(data_provider: Arc<dyn DataProvider>, partition_id: String) -> Self {
        StakeSnapshotCache {
            local: LocalCache::new(),
            data_provider,
            partition_id,
        }
    }

    /// Get a stake snapshot for the given epoch.
    ///
    /// Tries the local cache first. If not found, fetches from DataProvider
    /// and caches the result for future lookups.
    pub fn get(&self, epoch: u64) -> Option<StakeSet> {
        // Tier 1: Local cache hit
        if let Some(snapshot) = self.local.get(epoch) {
            return Some(snapshot);
        }

        // Tier 2: DataProvider fallback
        match self.data_provider.get_stake_snapshot(epoch, &self.partition_id) {
            Ok(snapshot) => {
                self.local.put(epoch, snapshot.clone());
                Some(snapshot)
            }
            Err(e) => {
                log::warn!("DataProvider returned error for epoch {}: {:?}", epoch, e);
                None
            }
        }
    }

    /// Put a snapshot directly into the local cache.
    pub fn put(&self, epoch: u64, snapshot: StakeSet) {
        self.local.put(epoch, snapshot);
    }

    /// Returns the number of cached epochs.
    pub fn cached_count(&self) -> usize {
        self.local.len()
    }

    /// Invalidate all cached snapshots.
    ///
    /// Called on epoch transition to force a fresh DataProvider fetch
    /// for the next epoch's stake snapshot.
    pub fn invalidate_all(&self) {
        self.local.invalidate_all();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pneumatic_core::data::StubDataProvider;
    use pneumatic_core::epoch::StakeSet;

    use super::*;

    fn make_stake_set(stakes: Vec<(Vec<u8>, u64)>) -> StakeSet {
        StakeSet {
            stakers: stakes.into_iter().collect(),
        }
    }

    #[test]
    fn cache_empty_returns_none() {
        let data_provider = Arc::new(StubDataProvider::new());
        let cache = StakeSnapshotCache::new(data_provider, "test".into());
        assert!(cache.get(1).is_none());
    }

    #[test]
    fn cache_put_and_get() {
        let snapshot = make_stake_set(vec![(vec![1], 100), (vec![2], 200)]);
        let data_provider = Arc::new(StubDataProvider::new());
        let cache = StakeSnapshotCache::new(data_provider, "test".into());
        cache.put(5, snapshot.clone());
        let result = cache.get(5);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.total_stake(), 300);
    }

    #[test]
    fn cache_fallback_to_data_provider() {
        let snapshot = make_stake_set(vec![(vec![10], 500)]);
        let data_provider = Arc::new(
            StubDataProvider::new().with_stake_snapshot(3, snapshot.clone())
        );
        let cache = StakeSnapshotCache::new(data_provider, "test".into());

        // First call — DataProvider fallback, caches locally
        let result = cache.get(3);
        assert!(result.is_some());
        assert_eq!(result.unwrap().total_stake(), 500);

        // Second call — local cache hit
        assert_eq!(cache.cached_count(), 1);
        let result = cache.get(3);
        assert!(result.is_some());
    }

    #[test]
    fn cache_independent_epochs() {
        let data_provider = Arc::new(
            StubDataProvider::new()
                .with_stake_snapshot(1, make_stake_set(vec![(vec![1], 100)]))
                .with_stake_snapshot(2, make_stake_set(vec![(vec![2], 200)])),
        );
        let cache = StakeSnapshotCache::new(data_provider, "test".into());

        assert_eq!(cache.get(1).unwrap().total_stake(), 100);
        assert_eq!(cache.get(2).unwrap().total_stake(), 200);
        assert_eq!(cache.cached_count(), 2);
    }

    #[test]
    fn cache_invalidate_all_clears_snapshots() {
        let data_provider = Arc::new(
            StubDataProvider::new()
                .with_stake_snapshot(1, make_stake_set(vec![(vec![1], 100)]))
                .with_stake_snapshot(2, make_stake_set(vec![(vec![2], 200)])),
        );
        let cache = StakeSnapshotCache::new(data_provider, "test".into());

        cache.put(1, make_stake_set(vec![(vec![1], 100)]));
        cache.put(2, make_stake_set(vec![(vec![2], 200)]));
        assert_eq!(cache.cached_count(), 2);

        cache.invalidate_all();
        assert_eq!(cache.cached_count(), 0);

        // After invalidation, get falls back to DataProvider
        let result = cache.get(1);
        assert!(result.is_some());
        assert_eq!(result.unwrap().total_stake(), 100);
        assert_eq!(cache.cached_count(), 1); // Re-cached from DataProvider
    }
}
