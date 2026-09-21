---
id: pattern-two-tier-snapshot-cache
title: "Tiered epoch-snapshot cache — local Map → DataProvider (peer tier reserved)"
type: pattern
namespace: pneumatic
visibility: namespace
summary: "Epoch snapshots (stake/executor sets) served by a local Mutex<HashMap> tier (O(1)) with a ~1ms DataProvider fallback; sentinel variant reserves a peer tier; invalidated on epoch advance."
auto_inject: false
applicable_when: "Adding a new per-epoch data dependency to a role crate, optimizing snapshot reads, or wiring epoch-boundary invalidation"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the snapshot cache modules (finalizer/sentinel stake_snapshot_cache.rs, sentinel/executor_set_cache.rs) change tiers or invalidation"
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

A shared structural pattern across the worker crates for per-epoch data (stake sets, executor sets). The tiers, as documented in the module headers:

- **Finalizer** (`finalizer/src/stake_snapshot_cache.rs:48-60`): two tiers — (1) local `Mutex<HashMap>` hit, O(1), zero network latency, the happy path; (2) DataProvider fallback via TCP/UDS to the local data service, ~1ms. The cache is invalidated on epoch transition so a new epoch's snapshot is freshly fetched; `advance_epoch` calls `invalidate_all` (`finalizer/src/finalizer.rs:954-957`).
- **Sentinel** (`sentinel/src/stake_snapshot_cache.rs:1-8`, `sentinel/src/executor_set_cache.rs:1-8`): three tiers — local (loaded when the first block of a new epoch is seen), DataProvider, and a **reserved** third peer tier ("request from another node's DataProvider"). The executor-set cache "mirrors StakeSnapshotCache pattern" verbatim.

Properties worth reusing: one cache per (epoch, snapshot-kind) pair keyed by epoch number; all I/O behind the `DataProvider` trait so tests inject in-memory fakes; invalidation tied to the epoch clock rather than TTL; and the sentinel's explicit tier-3 reservation so the data path can degrade from local-service to peer fetch without a shape change.
