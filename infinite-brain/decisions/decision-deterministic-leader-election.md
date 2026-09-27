---
id: decision-deterministic-leader-election
title: "ADR-003: Deterministic leader election via seeded RNG"
type: decision
namespace: pneumatic
visibility: namespace
summary: "LeaderSelector seeds a StdRng with a SHA-256 digest and walks a sorted stake set — a pure function of (StakeSet, epoch inputs), because thread_rng() would let identical nodes elect different leaders and break consensus."
auto_inject: false
applicable_when: "Changing leader election, the election seed, or the stake-set walk order"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the seed input changes (it has already grown: domain ‖ epoch ‖ prev_block_hash ‖ extra — see concept-leader-election)"
tags: [adr, design-decision, leader-election, determinism, consensus]
edges:
  - target: concept-leader-election
    type: supports
    weight: 0.95
    note: "The implemented form: domain-separated, tip-bound seed over the sorted stake walk"
  - target: concept-epoch-and-staking
    type: related_to
    weight: 0.7
    note: "Election consumes the epoch StakeSet"
  - target: decision-deterministic-per-tx-routing
    type: related_to
    weight: 0.7
    note: "Per-tx routing reuses the same seeded-walk algorithm with a per-transaction seed"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated verbatim from README ADR-003 when the README was trimmed (09/26/2026)"
related: []
source_url: "repo:README.md (former lines 116-120)"
---

# ADR-003: Deterministic leader election via seeded RNG

`LeaderSelector::select()` seeds a `StdRng` with a `SHA-256` digest and walks a **sorted** stake set.

**Rationale**: `rand::thread_rng()` is non-reproducible — two nodes with identical state would pick different leaders, making consensus impossible. The SHA-256 seed + sorted walk is a pure function: identical inputs always yield the same leader. The seed was later upgraded to be domain-separated and include `prev_block_hash` for forward-security (current form in `concept-leader-election`).
