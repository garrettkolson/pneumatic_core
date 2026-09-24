//! EpochSnapshotCache tests: empty/put/get, fetch-hook fallback, epoch
//! isolation, invalidate-all, the ExecutorSet instantiation, and the
//! partition-id propagation to the fetch hook.
use super::helpers::*;
use super::super::*;

// --- EpochSnapshotCache tests (shared tiered cache; was duplicated in
//     the sentinel/finalizer worker crates) ---

#[test]
fn snapshot_cache_empty_returns_none() {
    let dp = Arc::new(StubDataProvider::new());
    let cache: EpochSnapshotCache<StakeSet> =
        EpochSnapshotCache::new("test".into(), move |epoch, _p| dp.get_stake_snapshot(epoch, _p));
    assert!(cache.get(1).is_none());
}

#[test]
fn snapshot_cache_put_and_get() {
    let dp = Arc::new(StubDataProvider::new());
    let cache: EpochSnapshotCache<StakeSet> =
        EpochSnapshotCache::new("test".into(), move |epoch, _p| dp.get_stake_snapshot(epoch, _p));
    cache.put(5, make_stake_set(vec![(vec![1], 100), (vec![2], 200)]));
    let result = cache.get(5).unwrap();
    assert_eq!(result.total_stake(), 300);
}

#[test]
fn snapshot_cache_fallback_to_data_provider() {
    let snapshot = make_stake_set(vec![(vec![10], 500)]);
    let dp = Arc::new(StubDataProvider::new().with_stake_snapshot(3, snapshot));
    let cache: EpochSnapshotCache<StakeSet> =
        EpochSnapshotCache::new("test".into(), move |epoch, _p| dp.get_stake_snapshot(epoch, _p));

    // First call — fetch fallback, caches locally
    let result = cache.get(3).unwrap();
    assert_eq!(result.total_stake(), 500);

    // Second call — local cache hit
    assert_eq!(cache.cached_count(), 1);
    assert!(cache.get(3).is_some());
}

#[test]
fn snapshot_cache_independent_epochs() {
    let dp = Arc::new(
        StubDataProvider::new()
            .with_stake_snapshot(1, make_stake_set(vec![(vec![1], 100)]))
            .with_stake_snapshot(2, make_stake_set(vec![(vec![2], 200)])),
    );
    let cache: EpochSnapshotCache<StakeSet> =
        EpochSnapshotCache::new("test".into(), move |epoch, _p| dp.get_stake_snapshot(epoch, _p));

    assert_eq!(cache.get(1).unwrap().total_stake(), 100);
    assert_eq!(cache.get(2).unwrap().total_stake(), 200);
    assert_eq!(cache.cached_count(), 2);
}

#[test]
fn snapshot_cache_invalidate_all_clears_and_refetches() {
    let dp = Arc::new(
        StubDataProvider::new()
            .with_stake_snapshot(1, make_stake_set(vec![(vec![1], 100)]))
            .with_stake_snapshot(2, make_stake_set(vec![(vec![2], 200)])),
    );
    let cache: EpochSnapshotCache<StakeSet> =
        EpochSnapshotCache::new("test".into(), move |epoch, _p| dp.get_stake_snapshot(epoch, _p));

    cache.put(1, make_stake_set(vec![(vec![1], 100)]));
    cache.put(2, make_stake_set(vec![(vec![2], 200)]));
    assert_eq!(cache.cached_count(), 2);

    cache.invalidate_all();
    assert_eq!(cache.cached_count(), 0);

    // After invalidation, get falls back to the fetch hook
    assert_eq!(cache.get(1).unwrap().total_stake(), 100);
    assert_eq!(cache.cached_count(), 1); // Re-cached from DataProvider
}

#[test]
fn snapshot_cache_executor_set_variant() {
    let executors = make_executor_set(vec![(vec![10], 500)]);
    let dp = Arc::new(StubDataProvider::new().with_executor_set(3, executors));
    let cache: EpochSnapshotCache<ExecutorSet> =
        EpochSnapshotCache::new("test".into(), move |epoch, _p| dp.get_executor_set(epoch, _p));

    // Fetch fallback, then local hit
    assert_eq!(cache.get(3).unwrap().total_stake(), 500);
    assert_eq!(cache.cached_count(), 1);

    // put + invalidate_all on the executor-set variant
    cache.put(4, make_executor_set(vec![(vec![1], 100)]));
    assert_eq!(cache.cached_count(), 2);
    cache.invalidate_all();
    assert_eq!(cache.cached_count(), 0);
}

#[test]
fn snapshot_cache_fetch_receives_partition_id() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use crate::data::DataError;

    let saw_partition = Arc::new(AtomicBool::new(false));
    let saw_partition_clone = Arc::clone(&saw_partition);
    let cache: EpochSnapshotCache<StakeSet> =
        EpochSnapshotCache::new("partition-A".into(), move |_epoch, partition| {
            if partition == "partition-A" {
                saw_partition_clone.store(true, Ordering::SeqCst);
            }
            Err(DataError::DataNotFound)
        });
    assert!(cache.get(1).is_none()); // DataNotFound → None
    assert!(saw_partition.load(Ordering::SeqCst));
}
