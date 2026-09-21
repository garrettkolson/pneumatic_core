---
id: concept-epoch-and-staking
title: "Epochs, stake sets, and (stubbed) staking persistence"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Epoch (epoch.rs:20) + StakeSet/ExecutorSet drive leader election and sharding; IStakingManager/IEpochReconciler exist but StubStakingManager is a no-op (epoch.rs:492)."
auto_inject: false
applicable_when: "Working on epochs, stake accounting, slashing/rewards, reconciler, or executor sharding inputs"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If StubStakingManager/StubEpochReconciler are replaced by real implementations, or StakeSet/ExecutorSet shapes change"
tags: [epoch, staking, slashing, executor-set, stubbed]
edges:
  - target: concept-leader-election
    type: depends_on
    weight: 0.8
    note: "Epoch carries the leader_public_key chosen from its StakeSet (epoch.rs:28, 536-551)"
  - target: concept-executor-sharding
    type: related_to
    weight: 0.85
    note: "StakeSet.to_executor_set (epoch.rs:110) seeds the shard-aware executor pool"
  - target: concept-candidate-registry-conflict
    type: related_to
    weight: 0.75
    note: "Conflict struct (epoch.rs:55) and EpochReconciliation.finalization_conflicts (epoch.rs:78) model same-height disagreements"
  - target: source-protocol-changes
    type: related_to
    weight: 0.5
    note: "Epoch-boundary reconciliation is the protocol layer these changes target"
related: []
source_url: "Empty"
---

# Epochs, stake sets, and (stubbed) staking persistence

`Epoch` (`src/epoch.rs:20`) is a time-bounded period (`start_timestamp`/`end_timestamp`), numbered, carrying the epoch's `leader_public_key`; `Epoch::new_with_leader` (line 536) builds one by running an `IEpochLeaderSelector` over a stake set.

`StakeSet` (line 91) maps public key → stake for leader selection and quorum checks, with saturating `total_stake` (line 97), BTreeMap-routed `canonical_bytes` (line 123) and a SHA-256 `fingerprint` for persisted snapshots (line 135). `ExecutorSet` (line 150) is the shard-aware form; `StakeSet::to_executor_set` (line 110) converts at epoch boundaries for shard assignment.

Epoch-boundary work is split by trait: `IEpochReconciler` examines chain state and **returns** an `EpochReconciliation` (misshapen tokens, finalization `Conflict`s with per-side stakes at line 55, `slashing_ops`, `reward_ops` — lines 74-83) without mutating state; `IStakingManager` applies the ops (lines 456-465). `StakingOp` covers AddStaker/RemoveStaker/Slash/Reward (line 38).

**Caveat — stubbed:** the only implementations are `StubEpochReconciler` (returns empty reconciliation, line 483) and `StubStakingManager` (line 492), whose `apply_ops` is a documented no-op — it does **not** persist staking changes. Slash/reward effects are therefore not durable end-to-end until a real staking manager lands.
