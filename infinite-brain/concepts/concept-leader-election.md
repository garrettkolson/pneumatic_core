---
id: concept-leader-election
title: "Leader election: stake-weighted, domain-separated, tip-bound"
type: concept
namespace: pneumatic
visibility: namespace
summary: "LeaderSelector elects per epoch via stake-weighted selection (epoch/leader.rs:108) over a seed SHA-256(domain‖epoch‖prev_hash‖extra); BlockProposer proposes from the pending pool."
auto_inject: false
applicable_when: "Changing leader/finalizer/shard selection, selection seeds, or block proposal"
confidence: 1.0
verified_at: "09/23/2026"
verified_by: "dsh-agent"
staleness_signal: "If domain constants, derive_selection_seed layout, or deterministic_select walk changes"
tags: [leader-election, determinism, stake-weighted, proposal]
edges:
  - target: concept-epoch-and-staking
    type: depends_on
    weight: 0.85
    note: "Selection runs per epoch over the epoch's StakeSet"
  - target: concept-executor-sharding
    type: related_to
    weight: 0.7
    note: "Shares the domain-separated seed scheme: LEADER 0x01 / SHUFFLE 0x02 / FINALIZER 0x03 / SHARD 0x04 (epoch/leader.rs:18-21)"
  - target: concept-block-and-factory
    type: related_to
    weight: 0.75
    note: "Elected leader's key becomes proposer_key, bound into the block hash and conflict slash-target"
  - target: concept-optimistic-finality
    type: related_to
    weight: 0.65
    note: "Stake-weighted selection is what makes higher-stake competing proposals meaningful in conflict resolution"
related: []
source_url: "Empty"
---

# Leader election: stake-weighted, domain-separated, tip-bound

`LeaderSelector` (`epoch/leader.rs:252`) implements `IEpochLeaderSelector` (types.rs:98) by delegating to `deterministic_select` (leader.rs:108) with `LEADER_DOMAIN = 0x01`. Selection is stake-weighted: derive a 32-byte seed, pick a uniform target in `[0, total_stake)`, then walk stakers in **sorted lexicographic key order** with a saturating cumulative sum; zero-stake keys are skipped so a slashed-to-zero node can never be elected (leader.rs:124-152).

The seed is the key design: `derive_selection_seed` (`epoch/leader.rs:29`) hashes `SHA-256(domain ‖ epoch_number (big-endian) ‖ prev_block_hash ‖ extra)`. Two properties follow: (1) **domain separation** — a leader seed can't be replayed as a shard index; (2) **tip binding** — the choice is only knowable once the previous block is mined, not from public epoch+stake data alone. Every node with the same stake set and chain tip derives the same leader.

`BlockProposer` (`epoch/proposer.rs:13`) is the elected leader's side: it holds `leader_address`, `leader_stake`, `leader_hash`, and `IBlockProposer::propose_batch` (proposer.rs:45) dequeues validated transactions **per token** from the `PendingTransactionRegistry` and wraps each in a `SignedTransaction` stamped with the leader identity (`proposer_key = leader_address`) — the identity later bound into the block hash and consulted for slashing.
