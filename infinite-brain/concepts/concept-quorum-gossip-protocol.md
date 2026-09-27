---
id: concept-quorum-gossip-protocol
title: "Quorum gossip protocol — BlockFinalized / BlockConfirmed / BlockQuorumReached"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Three-message stake-weighted gossip that upgrades blocks Optimistic → Confirmed: the finalizer broadcasts block+StakeSet (BlockFinalized), peers vote (BlockConfirmed: hash + sender key), and the Committer cascades BlockQuorumReached at ≥ quorum% of stake."
auto_inject: false
applicable_when: "Working the quorum handlers, block confirmation, or finality status transitions"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the three message names, the per-voter accumulation, or the quorum threshold logic change"
tags: [quorum, gossip, finality, messages, consensus]
edges:
  - target: concept-optimistic-finality
    type: part_of
    weight: 0.9
    note: "This gossip is the confirmation/dispute tail of optimistic finality"
  - target: decision-optimistic-finality
    type: related_to
    weight: 0.9
    note: "ADR-005/010 repurposed the quorum machinery into this protocol"
  - target: concept-committer-role
    type: related_to
    weight: 0.8
    note: "quoruming.rs: handle_block_confirmed_vote / handle_block_quorum_reached / broadcast helpers"
  - target: concept-finalizer-role
    type: related_to
    weight: 0.8
    note: "BlockFinalized originates here, carrying the epoch stake set"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.6
    note: "Protocol detail (former README Phase 5d) migrated out when the README was trimmed (09/26/2026)"
related: []
source_url: "Empty"
---

# Quorum gossip protocol — Optimistic → Confirmed

A stake-weighted quorum gossip protocol upgrades blocks from `Optimistic` to `Confirmed` when a supermajority (default 67%) of stake-holders validate and vote. Three message types:

1. **`BlockFinalized`** — the Finalizer broadcasts the block **plus the full `StakeSet`** (the only message that carries one). Recipients: committers + archivars.
2. **`BlockConfirmed`** — a peer's vote: `(block_hash, sender_key)`. Each committer looks up the sender's stake from the cached stake set (never self-reported — C1), accumulates per unique voter in `confirmation_votes: Mutex<HashMap<hash, (HashSet<keys>, stake)>>`, and checks `voting_stake ≥ total_stake × quorum %`.
3. **`BlockQuorumReached`** — broadcast by a committer once quorum is reached; receivers find the block by hash (`Blockchain::get_block_at`) and transition `finality_status = Confirmed` (`set_finality_status`).

Flow: Finalizer → committers (`BlockFinalized`); each committer broadcasts its own vote (`broadcast_vote`), accumulates peers' votes (`handle_block_confirmed_vote`), and on quorum broadcasts `BlockQuorumReached` (`broadcast_quorum_reached` → `handle_block_quorum_reached`), cascading confirmation to all nodes. The original `BlockConfirmed` name was renamed to `BlockFinalized` when this protocol was added; the vote message took over the old name.
