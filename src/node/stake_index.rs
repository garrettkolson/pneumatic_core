//! `StakeIndex` — an off-thread current-epoch stake cache backing the
//! registration stake gate.
//!
//! # Why this exists (AUDIT Phase 4.4 / H7, H8)
//!
//! The registration gate (`NodeRegistry::handle_register`, invoked from
//! `NodeRegistry::handle_control` → the committer's `on_packet` closure → one of
//! the 4 plain `std::thread` RNS workers in `rns::wrapper::worker_loop`, which
//! have no Tokio runtime) must NOT perform a blocking data-service round-trip:
//! `DefaultDataProvider::get_user` does a framed TCP/UDS read, and a hung data
//! service would hold a worker for the full read timeout. Four concurrent
//! registrations exhaust the 4-thread pool and wedge the whole transport.
//!
//! This type eliminates the hot-path socket access entirely. A single
//! background `std::thread` periodically loads the current-epoch `StakeSet`
//! snapshot into an in-process `pubkey -> stake` index; the registration gate
//! then becomes a pure in-memory map lookup (zero I/O). Registration data is
//! the current-epoch snapshot (eventually consistent), not live per-user.
//!
//! # Fail closed
//!
//! - A cache miss (`key` absent) ⇒ `0` stake ⇒ the gate rejects. A cold or
//!   unwarmed index therefore blocks registrations rather than admitting them.
//! - A refresh error (data service down / snapshot not found) ⇒ the index is
//!   left unchanged, so the stale (or still-empty) index remains and the gate
//!   fails closed until the next successful refresh.
//!
//! `StakeCheck`'s signature is unchanged (`registry::StakeCheck`); only its
//! backing closure changed — the control-plane wire path is byte-for-byte
//! identical.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use dashmap::DashMap;

use crate::config::Config;
use crate::data::DataProvider;
use crate::node::registry::StakeCheck;
use crate::node::NodeRegistryType;

/// Default background refresh interval for the stake snapshot.
const DEFAULT_REFRESH_MS: u64 = 5_000;

/// In-process `pubkey -> current-epoch stake` index, refreshed off the
/// registration hot path by a single background thread.
///
/// Build the block-free registration gate with [`make_check`]; start and stop
/// the refresher with [`start`] / [`stop`]; keep it fresh across epochs with
/// [`set_epoch`].
pub struct StakeIndex {
    /// pubkey -> current-epoch stake. Read-only on the gate path via
    /// [`make_check`]; refreshed in place by [`refresh`].
    inner: Arc<DashMap<Vec<u8>, u64>>,
    /// The data service endpoint the refresher polls for the snapshot.
    data_provider: Arc<dyn DataProvider>,
    /// Token partition the current-epoch snapshot is stored under
    /// (`token_partition_id`).
    partition: String,
    /// The current epoch the refresher targets. Advanced by [`set_epoch`].
    epoch: Arc<RwLock<u64>>,
    /// Background refresh interval (ms).
    refresh_ms: u64,
    /// Set by [`stop`]; the refresher exits when this is true.
    shutdown: Arc<AtomicBool>,
    /// Background refresher handle, joined by [`stop`].
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl StakeIndex {
    /// Construct a `StakeIndex` without starting the background refresher.
    /// Call [`warm`] to load the initial snapshot, then [`start`] to spawn the
    /// refresher.
    pub fn new(
        data_provider: Arc<dyn DataProvider>,
        partition: String,
        epoch: u64,
        refresh_ms: Option<u64>,
    ) -> Self {
        StakeIndex {
            inner: Arc::new(DashMap::new()),
            data_provider,
            partition,
            epoch: Arc::new(RwLock::new(epoch)),
            refresh_ms: refresh_ms.unwrap_or(DEFAULT_REFRESH_MS),
            shutdown: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
    }

    /// Spawn the single background refresher. Idempotent — a second call is a
    /// no-op (the existing handle is left running). Returns `&Self` for
    /// chaining.
    pub fn start(&self) -> &Self {
        if self.handle.lock().unwrap().is_some() {
            return self;
        }
        let inner = Arc::clone(&self.inner);
        let data_provider = Arc::clone(&self.data_provider);
        let partition = self.partition.clone();
        let epoch = Arc::clone(&self.epoch);
        let refresh_ms = self.refresh_ms;
        let shutdown = Arc::clone(&self.shutdown);
        let handle = thread::spawn(move || {
            refresher_loop(inner, data_provider, partition, epoch, refresh_ms, shutdown);
        });
        *self.handle.lock().unwrap() = Some(handle);
        self
    }

    /// Run one synchronous refresh now, loading the current-epoch snapshot into
    /// the index. Used to warm the index before the network starts so a cold
    /// cache fails registrations closed rather than open. A failure leaves the
    /// index unchanged (stale ⇒ gate fails closed) and is logged.
    pub fn warm(&self) {
        match self.refresh() {
            Ok(()) => {}
            Err(e) => eprintln!("[pneumatic] stake_index: warm-up failed: {e}"),
        }
    }

    /// Build the block-free registration gate. The returned closure does only
    /// in-memory map lookups and cheap config reads — it never touches the data
    /// service, so a hung data service cannot wedge the RNS worker pool.
    pub fn make_check(&self, config: Arc<Config>) -> StakeCheck {
        let inner = Arc::clone(&self.inner);
        Arc::new(move |key: &[u8], node_type: &NodeRegistryType| {
            let global_min = config.get_global_min_stake();
            let type_min = config.get_min_type_stake(node_type);
            // Cache miss ⇒ 0 stake ⇒ the gate rejects (fail closed).
            let mine = inner.get(key).map(|r| *r).unwrap_or(0);
            crate::config::meets_minimum_stake(mine, global_min, type_min)
        })
    }

    /// Point the refresher at a new epoch and force one refresh so the gate
    /// consults the current-epoch snapshot. Called from the committer's epoch
    /// advance (the single source of truth).
    pub fn set_epoch(&self, epoch: u64) {
        *self.epoch.write().unwrap() = epoch;
        let _ = self.refresh();
    }

    /// Set the shutdown flag and join the refresher (mirrors
    /// `NodeRegistry::stop_eviction`).
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    /// Run one refresh: read the current-epoch snapshot and **replace** the
    /// index contents. The DashMap is updated in place (remove dropped keys,
    /// insert new) so readers holding `&self.inner` never observe a map built
    /// from an intermediate snapshot. A key evicted from the old snapshot can
    /// briefly miss during the update window — that fails the gate closed, the
    /// desired direction.
    fn refresh(&self) -> Result<(), String> {
        let epoch = *self.epoch.read().unwrap();
        let new_map = build_index(self.data_provider.as_ref(), &self.partition, epoch)?;
        replace_inner_map(&self.inner, new_map);
        Ok(())
    }
}

/// Update `inner` in place so that it holds exactly `new`: remove keys dropped
/// since the last snapshot, then add or overwrite the current keys. Keys still
/// present are updated in place; only dropped keys can briefly miss during the
/// update, which fails the gate closed. The DashMap is a concurrent map, so
/// this is safe against gate readers doing point lookups.
fn replace_inner_map(
    inner: &DashMap<Vec<u8>, u64>,
    new: DashMap<Vec<u8>, u64>,
) {
    let stale: Vec<Vec<u8>> = inner
        .iter()
        .filter(|kv| !new.contains_key(kv.key()))
        .map(|kv| kv.key().clone())
        .collect();
    for key in stale {
        inner.remove(&key);
    }
    for kv in &new {
        inner.insert(kv.key().clone(), *kv.value());
    }
}

/// Read the current-epoch snapshot from the data service into a fresh index
/// table. On error the caller leaves the existing index unchanged.
fn build_index(
    data_provider: &dyn DataProvider,
    partition: &str,
    epoch: u64,
) -> Result<DashMap<Vec<u8>, u64>, String> {
    let snapshot = data_provider
        .get_stake_snapshot(epoch, partition)
        .map_err(|e| format!("get_stake_snapshot({epoch}, {partition}): {e}"))?;
    let mut index: DashMap<Vec<u8>, u64> = DashMap::new();
    for (key, stake) in snapshot.stakers {
        index.insert(key, stake);
    }
    Ok(index)
}

/// Background refresher loop. Sleep-polls the data service on its own `std`
/// thread (like the registry eviction loop and the RNS workers), exits cleanly
/// on shutdown, and never panics on a data error (a stale index ⇒ fail closed).
fn refresher_loop(
    inner: Arc<DashMap<Vec<u8>, u64>>,
    data_provider: Arc<dyn DataProvider>,
    partition: String,
    epoch: Arc<RwLock<u64>>,
    refresh_ms: u64,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let current_epoch = *epoch.read().unwrap();
        match build_index(data_provider.as_ref(), &partition, current_epoch) {
            Ok(new_map) => replace_inner_map(&inner, new_map),
            Err(e) => eprintln!("[pneumatic] stake_index: refresh failed (index unchanged): {e}"),
        }
        thread::sleep(Duration::from_millis(refresh_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::data::{DataError, StubDataProvider};
    use crate::epoch::{ExecutorSet, StakeSet};
    use crate::node::NodeTypeConfig;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Instant;
    use strum::IntoEnumIterator;

    /// Build a test `Config` with uniform floors (10) except where
    /// `per_type` overrides a type's minimum. Global floor is the cost-model
    /// default (10) — the environment registry is empty in this fixture, so
    /// `Config::get_global_min_stake` falls back to the default.
    fn test_config(per_type: HashMap<NodeRegistryType, u64>) -> Arc<Config> {
        let mut type_configs = DashMap::new();
        for t in NodeRegistryType::iter() {
            let floor = per_type.get(&t).copied().unwrap_or(10);
            type_configs.insert(t, NodeTypeConfig { min: 1, max: 10, min_stake: floor });
        }
        Arc::new(Config::new_for_testing(
            "test".to_string(),
            Arc::new(DashMap::new()),
            Arc::new(type_configs),
        ))
    }

    /// A data provider whose `get_stake_snapshot` blocks forever, simulating a
    /// hung data service. The gate must never call it. Only the four required
    /// `DataProvider` methods (the rest carry default impls) are implemented;
    /// the blocking method is the trap — a gate that reached for the data
    /// service would block on it, so a fast completion proves zero I/O.
    struct HangProvider;

    impl DataProvider for HangProvider {
        fn get_stake_snapshot(&self, _epoch: u64, _partition: &str) -> Result<StakeSet, DataError> {
            let (tx, rx) = mpsc::channel::<()>();
            let _ = rx.recv(); // never sent ⇒ blocks forever
            Ok(StakeSet::default())
        }
        fn save_stake_snapshot(&self, _epoch: u64, _snapshot: StakeSet, _partition: &str) -> Result<(), DataError> {
            Ok(())
        }
        fn get_executor_set(&self, _epoch: u64, _partition: &str) -> Result<ExecutorSet, DataError> {
            Err(DataError::DataNotFound)
        }
        fn save_executor_set(&self, _epoch: u64, _set: ExecutorSet, _partition: &str) -> Result<(), DataError> {
            Ok(())
        }
    }

    /// A data provider that counts `get_stake_snapshot` calls so a test can
    /// assert the gate performed zero I/O on the hot path.
    struct CountingProvider {
        inner: StubDataProvider,
        snapshot_calls: Arc<AtomicUsize>,
    }

    impl CountingProvider {
        fn new() -> Self {
            CountingProvider {
                inner: StubDataProvider::new(),
                snapshot_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl DataProvider for CountingProvider {
        fn get_stake_snapshot(&self, epoch: u64, partition: &str) -> Result<StakeSet, DataError> {
            self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.get_stake_snapshot(epoch, partition)
        }
        fn save_stake_snapshot(&self, epoch: u64, snapshot: StakeSet, partition: &str) -> Result<(), DataError> {
            self.inner.save_stake_snapshot(epoch, snapshot, partition)
        }
        fn get_executor_set(&self, epoch: u64, partition: &str) -> Result<ExecutorSet, DataError> {
            self.inner.get_executor_set(epoch, partition)
        }
        fn save_executor_set(&self, epoch: u64, set: ExecutorSet, partition: &str) -> Result<(), DataError> {
            self.inner.save_executor_set(epoch, set, partition)
        }
    }

    /// A test `DataProvider` delegating to a `StubDataProvider` so a snapshot
    /// can be swapped in place (Stub's `with_*` builders consume `self`). The
    /// `StakeIndex` holds the shared `Arc`, so `set_stake_snapshot` changes the
    /// current-epoch snapshot the refresher reads between warm and refresh.
    struct MutatingProvider {
        inner: Mutex<StubDataProvider>,
    }

    impl MutatingProvider {
        fn new(inner: StubDataProvider) -> Self {
            MutatingProvider { inner: Mutex::new(inner) }
        }
        fn set_stake_snapshot(&self, epoch: u64, snapshot: StakeSet) {
            let mut inner = self.inner.lock().unwrap();
            *inner = StubDataProvider::new().with_stake_snapshot(epoch, snapshot);
        }
    }

    impl DataProvider for MutatingProvider {
        fn get_stake_snapshot(&self, epoch: u64, partition: &str) -> Result<StakeSet, DataError> {
            self.inner.lock().unwrap().get_stake_snapshot(epoch, partition)
        }
        fn save_stake_snapshot(&self, epoch: u64, snapshot: StakeSet, partition: &str) -> Result<(), DataError> {
            self.inner.lock().unwrap().save_stake_snapshot(epoch, snapshot, partition)
        }
        fn get_executor_set(&self, epoch: u64, partition: &str) -> Result<ExecutorSet, DataError> {
            self.inner.lock().unwrap().get_executor_set(epoch, partition)
        }
        fn save_executor_set(&self, epoch: u64, set: ExecutorSet, partition: &str) -> Result<(), DataError> {
            self.inner.lock().unwrap().save_executor_set(epoch, set, partition)
        }
    }

    #[test]
    fn make_check_is_fail_closed_on_cache_miss() {
        let index = StakeIndex::new(
            Arc::new(StubDataProvider::new()),
            "token".to_string(),
            1,
            None,
        );
        let check = index.make_check(test_config(HashMap::new()));
        // No entry ⇒ 0 stake ⇒ rejected even though the floors are 10.
        let key = vec![1, 2, 3];
        assert!(!check(&key, &NodeRegistryType::Committer));
    }

    #[test]
    fn make_check_reads_warmed_snapshot() {
        let check = {
            let key = vec![9];
            let mut snapshot = StakeSet { stakers: HashMap::new() };
            snapshot.stakers.insert(key.clone(), 1000);
            let data = StubDataProvider::new().with_stake_snapshot(1, snapshot);
            let index = StakeIndex::new(Arc::new(data), "token".to_string(), 1, None);
            let check = index.make_check(test_config(HashMap::new())); // all floors 10, global 10
            index.warm();
            (check, key)
        };
        let (check, key) = check;
        assert!(check(&key, &NodeRegistryType::Committer));
    }

    #[test]
    fn make_check_enforces_both_global_and_type_min() {
        let key = vec![7];
        let mut snapshot = StakeSet { stakers: HashMap::new() };
        snapshot.stakers.insert(key.clone(), 50);
        let data = StubDataProvider::new().with_stake_snapshot(1, snapshot);
        let index = StakeIndex::new(Arc::new(data), "token".to_string(), 1, None);
        // Sentinel floor = 100; Committer floor = 10; global floor = 10.
        let mut per_type = HashMap::new();
        per_type.insert(NodeRegistryType::Sentinel, 100);
        let check = index.make_check(test_config(per_type));
        index.warm();

        // 50 < Sentinel floor 100 ⇒ rejected for Sentinel even though 50 >= 10.
        assert!(!check(&key, &NodeRegistryType::Sentinel));
        // 50 >= Committer floor 10 AND >= global 10 ⇒ admitted for Committer.
        assert!(check(&key, &NodeRegistryType::Committer));
    }

    #[test]
    fn refresh_replaces_snapshot_contents() {
        let data = Arc::new(MutatingProvider::new(StubDataProvider::new()));

        let mut s1 = StakeSet { stakers: HashMap::new() };
        s1.stakers.insert(vec![1], 100);
        data.set_stake_snapshot(1, s1);
        let index = StakeIndex::new(data.clone(), "token".to_string(), 1, None);
        index.warm();
        assert!(index.inner.contains_key(&vec![1]));
        assert!(!index.inner.contains_key(&vec![2]));

        // Second snapshot drops key 1 and adds key 2 ⇒ the table is *replaced*,
        // so key 1 must no longer be present.
        let mut s2 = StakeSet { stakers: HashMap::new() };
        s2.stakers.insert(vec![2], 200);
        data.set_stake_snapshot(1, s2);
        index.set_epoch(1);

        assert!(!index.inner.contains_key(&vec![1]));
        assert!(index.inner.contains_key(&vec![2]));
    }

    #[test]
    fn refresh_leaves_index_on_data_error() {
        // get_stake_snapshot for an unknown epoch returns DataNotFound ⇒ the
        // refresh fails ⇒ the index is left unchanged (fail closed).
        let key = vec![5];
        let mut s1 = StakeSet { stakers: HashMap::new() };
        s1.stakers.insert(key.clone(), 500);
        let data = StubDataProvider::new().with_stake_snapshot(1, s1);
        let index = StakeIndex::new(Arc::new(data), "token".to_string(), 1, None);
        index.warm();
        assert!(index.inner.contains_key(&key));

        index.set_epoch(2); // no snapshot at epoch 2 ⇒ refresh fails
        assert!(index.inner.contains_key(&key));
    }

    /// Primary regression (H7/H8). The gate closure must never touch the data
    /// service: drive 4 concurrent `make_check` calls against a `HangProvider`
    /// whose `get_stake_snapshot` blocks forever and assert they all return
    /// within a tight bound. Reverting the closure to the old inline
    /// `get_user`/snapshot round-trip would block for the hang ⇒ the bound
    /// fires.
    #[test]
    fn make_check_performs_no_io_on_hot_path() {
        let hang = HangProvider;
        let index = StakeIndex::new(Arc::new(hang), "token".to_string(), 1, None);
        let check = index.make_check(test_config(HashMap::new()));

        let start = Instant::now();
        let key = vec![1];
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let check = Arc::clone(&check);
                let key = key.clone();
                thread::spawn(move || {
                    (0..10_000)
                        .map(|_| check(&key, &NodeRegistryType::Committer))
                        .count()
                })
            })
            .collect();
        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 4 * 10_000);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "make_check took {:?}; expected zero hot-path I/O",
            start.elapsed()
        );
    }

    #[test]
    fn make_check_never_calls_the_data_service() {
        // A gate that reached for the data service would increment this counter
        // (and fail closed on its DataNotFound). The count must stay 0. The
        // index is left cold: a cache miss fails the gate closed (false) while
        // still performing zero I/O — which is exactly what this test proves.
        let counter = Arc::new(CountingProvider::new());
        let snapshot_calls = Arc::clone(&counter.snapshot_calls);
        let dp: Arc<dyn DataProvider> = counter.clone();
        let index = StakeIndex::new(dp, "token".to_string(), 1, None);
        let check = index.make_check(test_config(HashMap::new()));
        let key = vec![1];
        for _ in 0..1000 {
            assert!(!check(&key, &NodeRegistryType::Committer)); // cache miss ⇒ false
        }
        assert_eq!(0, snapshot_calls.load(Ordering::SeqCst));
    }
}
