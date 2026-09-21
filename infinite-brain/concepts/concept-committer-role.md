---
id: concept-committer-role
title: "Committer role — terminal commit pipeline and epoch management"
type: concept
namespace: pneumatic
visibility: namespace
summary: "The terminal node: role-gated auth for 7 wire actions, commit with conflict resolution + per-sender gas, block distribution, and the epoch loop (staking, reconciliation, leader proposal)."
auto_inject: false
applicable_when: "Working on committer/, commit-time validation, conflict resolution, gas, epoch transitions, or block distribution"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when committer/src/committer.rs changes its action router, handle_commit pipeline, or conflict outcome enum"
tags: [committer, worker-crate, commit, conflict-resolution, epochs]
edges:
  - target: concept-candidate-registry-conflict
    type: related_to
    weight: 0.9
    note: "handle_conflict_at_commit implements the append / rollback-then-append / loser-discarded outcomes"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.8
    note: "Terminal commit to the token blockchains that make up the lattice"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.9
    note: "authenticate_message, H12 payload hash check, and surfaced gas/snapshot errors all fail closed"
  - target: concept-transaction-lifecycle
    type: related_to
    weight: 0.8
    note: "Re-runs the shielded validation spec authoritatively at commit time (reserved S5.3 gate)"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.8
    note: "Inbound bus is 7 action strings: Commit, DistributeToken, DistributeBlock, EpochReconcile, BlockFinalized, BlockConfirmed, BlockQuorumReached"
related: []
source_url: "Empty"
---

# Committer role — terminal commit pipeline and epoch management

The Committer is the terminal node in the pneumatic pipeline (`committer/src/committer.rs:85-96`): it receives `TransactionCommit` messages from Finalizers, validates and commits blocks to token blockchains, distributes blocks to archivers, handles token distribution, and manages epoch transitions (staking, reconciliation, leader selection).

Commit pipeline (code-verified):

- **Action router**: `handle_message` first runs `authenticate_message` — envelope signature check, then registration lookup, then a role gate (`allowed_senders_for`: Commit/BlockFinalized only from Finalizers, Distribute* only from Committers, `BlockConfirmed`/`BlockQuorumReached` from any registered node, EpochReconcile self-only) — then dispatches the 7 actions (`committer.rs:247-265`, `267-335`).
- **`handle_commit`** → `check_and_commit_transaction_results`: accepts `Finalizing` (standard pipeline) or `Validated` (leader-proposal) state; materializes a missing pending-registry entry from the wire block (AUDIT 4.1/H4 sink path, `committer.rs:476-487`); hash-compares the validated transaction against the wire block's embedded one (H12 payload-mismatch gate, `committer.rs:519-521`); resolves conflicts via `handle_conflict_at_commit` into `Commit` / `CommitWinnerAfterRollback(loser_hash)` / `LoserDiscarded` (`committer.rs:523-535`); deducts gas from the sender's fuel balance under a per-sender RMW mutex, failing closed on any data error (M11, `committer.rs:537-562`).
- **`BlockServices::commit_block`** applies the block through `Token::commit_block` (validate, append, sequence) with optional loser-tip rollback, then distributes to archivers (`block_services.rs:67-140`).
- **Orphan buffering**: out-of-order `BlockFinalized` blocks buffer per-token with a global cap and TTL, replayed as the tip advances (AUDIT 3.4/H15 — `committer/src/orphan_buffer.rs:1-20`).
- **Epoch loop**: `run_epoch_loop` / `propose_blocks` drive leader proposal, reconciliation, and snapshots (`committer.rs:1347`, `1440`); `epoch_manager.rs` provides concrete `StakeStore`, `StakingManager`, `LeaderSelector`, `EpochReconciler` replacing the core stubs.
