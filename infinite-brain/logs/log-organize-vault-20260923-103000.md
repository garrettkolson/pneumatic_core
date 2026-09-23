---
id: log-organize-vault-20260923-103000
type: log
operation: organize-vault
date: 2026-09-23T10:30:00
namespace: pneumatic
summary: "Landed monolith-modularization step 1: core EpochSnapshotCache<T> replaces 3 byte-identical worker cache copies; suite green 823/32; baseline + pattern + task nodes updated."
affected_nodes: ["fact-test-suite", "fact-worker-crate-tests", "pattern-two-tier-snapshot-cache", "task-monolith-modularization"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — EpochSnapshotCache<T> extraction

Refactor step 1 (task-monolith-modularization) landed:

- Added `pneumatic_core::epoch::EpochSnapshotCache<T: Clone + Send>` to `src/epoch.rs`:
  local `Mutex<HashMap<u64, T>>` tier + `Box<dyn Fn(u64, &str) -> Result<T, DataError>>`
  fetch hook + partition id; get/put/cached_count/invalidate_all; warn! on fetch error.
- Added `log = "0.4"` to core Cargo.toml (lockfile-only, zero new packages).
- Deleted `sentinel/src/stake_snapshot_cache.rs`, `sentinel/src/executor_set_cache.rs`,
  `finalizer/src/stake_snapshot_cache.rs` (the sentinel+finalizer stake caches were
  code-identical; executor-set differed only in element type/method name).
- Sentinel `new()` builds `Arc<EpochSnapshotCache<StakeSet>>` + `Arc<EpochSnapshotCache<ExecutorSet>>`;
  finalizer builds `EpochSnapshotCache<StakeSet>`; each fetch closure captures its own
  `Arc<dyn DataProvider>` clone (fixes the move-out-of-field conflict).
- 7 new core tests `epoch::tests::snapshot_cache_*` (both element types + partition pass-through).
- Suite: 823 passed / 32 ignored / 0 failed (delta vs 09/22 847/16 = 623c7f3's 16
  long-running ignores + this change's net −8; arithmetic recorded in fact-test-suite).
