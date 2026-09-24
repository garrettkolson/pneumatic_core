---
id: pattern-two-tier-snapshot-cache
title: "Tiered epoch-snapshot cache — local Map → DataProvider (peer tier reserved)"
type: pattern
namespace: pneumatic
visibility: namespace
summary: "SINGLE generic implementation since 09/23: core `epoch::EpochSnapshotCache<T>` (local Mutex<HashMap> tier, O(1), + fetch-hook ~1ms DataProvider fallback; peer tier reserved); sentinel/finalizer instantiate it with StakeSet/ExecutorSet; invalidated on epoch advance."
auto_inject: false
applicable_when: "Adding a new per-epoch data dependency to a role crate, optimizing snapshot reads, or wiring epoch-boundary invalidation"
confidence: 1.0
verified_at: "09/23/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when core src/epoch/snapshot_cache.rs EpochSnapshotCache changes tiers, fetch-hook signature, or invalidation semantics"
tags: [caching, snapshots, epoch, data-provider, performance]
edges:
  - target: concept-finalizer-role
    type: related_to
    weight: 0.8
    note: "Finalizer's two-tier StakeSnapshotCache feeds quorum gossip (BlockFinalized stake set) and resolve_previous_hash lookups"
  - target: concept-sentinel-role
    type: related_to
    weight: 0.85
    note: "Sentinel's three-tier StakeSnapshotCache + ExecutorSetCache back deterministic finalizer assignment and shard routing"
  - target: concept-executor-sharding
    type: related_to
    weight: 0.8
    note: "The executor-set cache is the data source for shard-aware routing decisions"
related: []
source_url: "Empty"
---

# Tiered epoch-snapshot cache — local Map → DataProvider (peer tier reserved)

**Single shared implementation** (09/23, monolith-modularization step 1): `pneumatic_core::epoch::EpochSnapshotCache<T>` in `src/epoch/snapshot_cache.rs` (moved from `src/epoch.rs` in the step-5 root-lib split). One generic struct replaces the three near-identical worker-crate copies that previously existed (sentinel + finalizer `stake_snapshot_cache.rs` — byte-identical code — and sentinel `executor_set_cache.rs`; all deleted).

Shape:

- **Tier 1 — local** `Mutex<HashMap<u64, T>>` (std sync Mutex; core has no parking_lot dep): O(1) hit, zero network latency, the happy path.
- **Tier 2 — fetch hook** `Box<dyn Fn(u64, &str) -> Result<T, DataError> + Send + Sync>`: called only on a local miss; the worker closures capture an `Arc<dyn DataProvider>` clone and call `get_stake_snapshot` / `get_executor_set` (~1 ms local data service). `get` re-caches a successful fetch; a fetch error logs `warn!` and returns `None` (fail-open to `None`, as before).
- **Tier 3 — peer (reserved)**: not wired; reserved for future degradation from local-service to peer fetch without a shape change.

Instantiation: sentinel `new()` builds `Arc<EpochSnapshotCache<StakeSet>>` and `Arc<EpochSnapshotCache<ExecutorSet>>` (`sentinel/src/sentinel.rs`); finalizer builds `EpochSnapshotCache<StakeSet>` (`finalizer/src/finalizer.rs`). Invalidation is tied to the epoch clock: finalizer `advance_epoch` / sentinel epoch-advance call `invalidate_all` (no TTL).

Properties worth reusing (unchanged): one cache per (epoch, snapshot-kind) pair keyed by epoch number; all I/O behind the `DataProvider` trait so tests inject in-memory fakes (the 7 `epoch::tests::snapshot_cache_*` tests cover both StakeSet and ExecutorSet variants plus partition-id pass-through).

History: the three-way duplication was flagged in the 09/23 refactoring-opportunity analysis (`task-monolith-modularization`) and removed as its step 1 — sentinel 67→57, finalizer 66→61 tests, core +7.
