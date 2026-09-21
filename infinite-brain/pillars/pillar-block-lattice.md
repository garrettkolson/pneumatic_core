---
id: pillar-block-lattice
title: "Block-lattice PoS consensus"
type: pillar
namespace: pneumatic
visibility: namespace
summary: "Per-token block lattice (Nano-style) with a 4-role worker pipeline: executor proposes, finalizer optimistically finalizes, committer commits, sentinel monitors."
auto_inject: true
applicable_when: "Any work touching block production, finality, per-token chains, or the worker role pipeline"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If finality reverts from optimistic commit back to quorum voting, or a 5th role is added"
tags: [consensus, block-lattice, pos, architecture]
edges:
  - target: concept-optimistic-finality
    type: depends_on
    weight: 1.0
    note: "Optimistic commit is the finality mechanism inside the lattice"
  - target: fact-workspace-layout
    type: derived_from
    weight: 0.9
    note: "The 4-role pipeline is implemented across the 7 workspace crates"
  - target: source-protocol-changes
    type: derived_from
    weight: 1.0
    note: "All 4 Phase-0 protocol decisions resolved in PROTOCOL_CHANGES.md"
  - target: pillar-shielded-value-transfer
    type: supports
    weight: 0.9
    note: "Shielded value transfer is the first deliverable riding on this lattice"
related: []
source_url: "Empty"
---

# Block-lattice PoS consensus

Pneumatic is a block-lattice proof-of-stake protocol: each token runs its own chain, and a transaction's effect is localized to one token's chain — the Nano-style "block lattice" model. There is no single global block; a token block is validated against its own chain state (`src/tokens.rs`, `src/blocks.rs`), and `BlockFactory::create_hash` computes a SHA-256 over `previous_hash || timestamp || signed_trans || token_metadata` (`src/blocks.rs:85`).

The protocol runs as a 4-role worker pipeline over a Rust workspace: **executor** (proposes/attests blocks, sharded per epoch), **finalizer** (optimistic finalization — the first executor signature finalizes), **committer** (on-chain commit), **sentinel** (monitoring). A composite `node-server` runtime hosts all four roles as plugins in one process for full-node deployments.

Tier-1 deliverable of the project: shielded value transfer (see roadmap), built as a new transaction type validated by the network-side `ShieldedVerifier`.
