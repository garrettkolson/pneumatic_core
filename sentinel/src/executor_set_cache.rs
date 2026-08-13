//! Executor set cache for deterministic shard-aware routing.
//!
//! Three-tier cache (mirrors StakeSnapshotCache pattern):
//! 1. **Local** (Mutex<HashMap>) — O(1), loaded when first block of new epoch is seen
//! 2. **DataProvider** — ~1ms, local service call
//! 3. **Peer** (reserved) — request from another node's DataProvider

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use pneumatic_core::data::DataProvider;
use pneumatic_core::epoch::ExecutorSet;

/// Local cache backed by a Mutex for lock-free concurrency.
struct LocalCache {
    /// epoch → executor set
    cache: Mutex<HashMap<u64, ExecutorSet>>,
}

impl LocalCache {
    fn new() -> Self {
        LocalCache {
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, epoch: u64) -> Option<ExecutorSet> {
        self.cache.lock().get(&epoch).cloned()
    }

    fn put(&self, epoch: u64, executors: ExecutorSet) {
        self.cache.lock().insert(epoch, executors);
    }

    fn len(&self) -> usize {
        self.cache.lock().len()
    }

    fn clear(&self) {
        self.cache.lock().clear();
    }
}

/// Three-tier executor set cache for sentinel shard-aware routing.
///
/// The sentinel needs an executor set to deterministically assign
/// transactions to executor shards. This cache provides the executor set
/// with:
///
/// 1. **Local cache hit** (O(1)): Executor set was loaded when a block from
///    this epoch was observed. This is the happy path — zero network latency.
///
/// 2. **DataProvider fallback (~1ms)**: If the local cache doesn't have it,
///    the sentinel calls the local DataProvider service via TCP/UDS.
///
/// 3. **Peer request (reserved)**: If the DataProvider is unavailable,
///    the sentinel can ask another node for their executor set.
///
/// The `epoch_number` from an incoming block serves as both the freshness
/// check and the lookup key.
pub struct ExecutorSetCache {
    local: LocalCache,
    data_provider: Arc<dyn DataProvider>,
    partition_id: String,
}

impl ExecutorSetCache {
    /// Create a new cache with no cached executor sets.
    pub fn new(data_provider: Arc<dyn DataProvider>, partition_id: String) -> Self {
        ExecutorSetCache {
            local: LocalCache::new(),
            data_provider,
            partition_id,
        }
    }

    /// Get an executor set for the given epoch.
    ///
    /// Tries the local cache first. If not found, fetches from DataProvider
    /// and caches the result for future lookups.
    pub fn get(&self, epoch: u64) -> Option<ExecutorSet> {
        // Tier 1: Local cache hit
        if let Some(executors) = self.local.get(epoch) {
            return Some(executors);
        }

        // Tier 2: DataProvider fallback
        match self.data_provider.get_executor_set(epoch, &self.partition_id) {
            Ok(executors) => {
                self.local.put(epoch, executors.clone());
                Some(executors)
            }
            Err(e) => {
                log::warn!("DataProvider returned error for executor_set epoch {}: {:?}", epoch, e);
                None
            }
        }
    }

    /// Put an executor set directly into the local cache.
    /// Called when a block from a new epoch is observed.
    pub fn put(&self, epoch: u64, executors: ExecutorSet) {
        self.local.put(epoch, executors);
    }

    /// Returns the number of cached epochs.
    pub fn cached_count(&self) -> usize {
        self.local.len()
    }

    /// Invalidate all cached executor sets.
    /// Called when epoch advances to force a fresh shuffle.
    pub fn invalidate_all(&self) {
        self.local.clear();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pneumatic_core::data::StubDataProvider;
    use pneumatic_core::epoch::ExecutorSet;

    use super::*;

    fn make_executor_set(stakes: Vec<(Vec<u8>, u64)>) -> ExecutorSet {
        ExecutorSet {
            executors: stakes.into_iter().collect(),
        }
    }

    #[test]
    fn cache_empty_returns_none() {
        let data_provider = Arc::new(StubDataProvider::new());
        let cache = ExecutorSetCache::new(data_provider, "test".into());
        assert!(cache.get(1).is_none());
    }

    #[test]
    fn cache_put_and_get() {
        let executors = make_executor_set(vec![(vec![1], 100), (vec![2], 200)]);
        let data_provider = Arc::new(StubDataProvider::new());
        let cache = ExecutorSetCache::new(data_provider, "test".into());
        cache.put(5, executors.clone());
        let result = cache.get(5);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn cache_fallback_to_data_provider() {
        let executors = make_executor_set(vec![(vec![10], 500)]);
        let data_provider = Arc::new(
            StubDataProvider::new().with_executor_set(3, executors.clone())
        );
        let cache = ExecutorSetCache::new(data_provider, "test".into());

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
                .with_executor_set(1, make_executor_set(vec![(vec![1], 100)]))
                .with_executor_set(2, make_executor_set(vec![(vec![2], 200)])),
        );
        let cache = ExecutorSetCache::new(data_provider, "test".into());

        assert_eq!(cache.get(1).unwrap().len(), 1);
        assert_eq!(cache.get(2).unwrap().len(), 1);
        assert_eq!(cache.cached_count(), 2);
    }

    #[test]
    fn cache_invalidate_all_clears() {
        let executors = make_executor_set(vec![(vec![1], 100)]);
        let data_provider = Arc::new(StubDataProvider::new());
        let cache = ExecutorSetCache::new(data_provider, "test".into());
        cache.put(5, executors);

        assert_eq!(cache.cached_count(), 1);
        cache.invalidate_all();
        assert_eq!(cache.cached_count(), 0);
    }
}
