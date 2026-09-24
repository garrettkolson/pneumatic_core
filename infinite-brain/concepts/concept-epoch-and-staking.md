---
id: concept-epoch-and-staking
title: "Epochs, stake sets, and (stubbed) staking persistence"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Epoch (epoch/types.rs:15) + StakeSet/ExecutorSet drive leader election and sharding; IStakingManager/IEpochReconciler exist but StubStakingManager is a no-op (epoch/types.rs:122)."
auto_inject: false
applicable_when: "Working on epochs, stake accounting, slashing/rewards, reconciler, or executor sharding inputs"
confidence: 1.0
verified_at: "09/23/2026"
verified_by: "dsh-agent"
staleness_signal: "If StubStakingManager/StubEpochReconciler are replaced by real implementations, or StakeSet/ExecutorSet shapes change"
tags: [epoch, staking, slashing, executor-set, stubbed]
edges:
  - target: concept-leader-election
    type: depends_on
    weight: 0.8
    note: "Epoch carries the leader_public_key chosen from its StakeSet (epoch/types.rs:23, 137)"
  - target: concept-executor-sharding
    type: related_to
    weight: 0.85
    note: "StakeSet.to_executor_set (epoch/stake_sets.rs:32) seeds the shard-aware executor pool"
  - target: concept-candidate-registry-conflict
    type: related_to
    weight: 0.75
    note: "Conflict struct (epoch/types.rs:50) and EpochReconciliation.finalization_conflicts (epoch/types.rs:73) model same-height disagreements"
  - target: source-protocol-changes
    type: related_to
    weight: 0.5
    note: "Epoch-boundary reconciliation is the protocol layer these changes target"
related: []
source_url: "Empty"
---

# Epochs, stake sets, and (stubbed) staking persistence

`Epoch` (`epoch/types.rs:15`) is a time-bounded period (`start_timestamp`/`end_timestamp`), numbered, carrying the epoch's `leader_public_key`; `Epoch::new_with_leader` (types.rs:137) builds one by running an `IEpochLeaderSelector` over a stake set.

`StakeSet` (`epoch/stake_sets.rs:13`) maps public key → stake for leader selection and quorum checks, with saturating `total_stake` (line 19), BTreeMap-routed `canonical_bytes` (line 45) and a SHA-256 `fingerprint` for persisted snapshots (line 57). `ExecutorSet` (line 72) is the shard-aware form; `StakeSet::to_executor_set` (line 32) converts at epoch boundaries for shard assignment.

Epoch-boundary work is split by trait: `IEpochReconciler` (types.rs:86) examines chain state and **returns** an `EpochReconciliation` (misshapen tokens, finalization `Conflict`s with per-side stakes at types.rs:50, `slashing_ops`, `reward_ops` — types.rs:69-78) without mutating state; `IStakingManager` (types.rs:92) applies the ops. `StakingOp` covers AddStaker/RemoveStaker/Slash/Reward (types.rs:33).

**Caveat — stubbed:** the only implementations are `StubEpochReconciler` (returns empty reconciliation, types.rs:113) and `StubStakingManager` (types.rs:122), whose `apply_ops` is a documented no-op — it does **not** persist staking changes. Slash/reward effects are therefore not durable end-to-end until a real staking manager lands.
