---
id: decision-dashmap-registries
title: "ADR-002: DashMap for concurrent registry state"
type: decision
namespace: pneumatic
visibility: namespace
summary: "NodeRegistry, CandidateRegistry, PendingTransactionRegistry, and StakeStore all back their state with DashMap — per-shard locking beats Mutex<HashMap> for read-heavy, multi-async access keyed by public-key bytes."
auto_inject: false
applicable_when: "Choosing a concurrent collection for new registry state"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when a registry migrates off DashMap or DashMap is replaced workspace-wide"
tags: [adr, design-decision, dashmap, concurrency, registry]
edges:
  - target: concept-node-registry
    type: supports
    weight: 0.9
    note: "Per-type DashMap directories are the primary instance"
  - target: concept-pending-tx-registry
    type: supports
    weight: 0.8
    note: "In-flight tx state lives in a DashMap plus an ordered per-token pool"
  - target: concept-candidate-registry-conflict
    type: supports
    weight: 0.8
    note: "Candidate collection at each chain position is DashMap-backed"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated verbatim from README ADR-002 when the README was trimmed (09/26/2026)"
related: []
source_url: "repo:README.md (former lines 110-114)"
---

# ADR-002: DashMap for concurrent registry state

Node registries (`NodeRegistry`, `CandidateRegistry`, `PendingTransactionRegistry`, `StakeStore`) all use `DashMap` as their backing store.

**Rationale**: A lock-free concurrent HashMap provides better throughput than `Mutex<HashMap>` for read-heavy workloads. All registries are keyed by `Vec<u8>` (public key bytes) and accessed from multiple async tasks. DashMap's per-shard locking avoids global contention.
