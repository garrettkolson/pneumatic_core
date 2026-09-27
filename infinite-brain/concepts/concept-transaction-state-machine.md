---
id: concept-transaction-state-machine
title: "Transaction state machine — explicit TransactionState pipeline"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Transactions move Pending → Preloaded → Validated → Executing → Finalizing → Committed (Failed reachable from any stage) via the explicit TransactionState enum; PendingTransaction holds an atomic lock count against premature collection."
auto_inject: false
applicable_when: "Adding a pipeline stage, debugging tx state transitions, or working the pending registry"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when TransactionState variants change or a stage is inserted/removed from the pipeline"
tags: [state-machine, transactions, pipeline, pending-registry]
edges:
  - target: concept-pending-tx-registry
    type: part_of
    weight: 0.9
    note: "The registry is where state lives and transitions are enforced"
  - target: pillar-block-lattice
    type: related_to
    weight: 0.8
    note: "The pipeline each transaction traverses before becoming a block"
  - target: concept-sentinel-role
    type: related_to
    weight: 0.7
    note: "Sentinel handlers drive the early transitions (Preload/Validate/assign)"
  - target: concept-committer-role
    type: related_to
    weight: 0.7
    note: "The commit gate accepts Finalizing (standard) or Validated (leader-proposal) state"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.6
    note: "Recorded in the README Consensus Flow section, migrated out when the README was trimmed (09/26/2026)"
related: []
source_url: "Empty"
---

# Transaction state machine — explicit TransactionState pipeline

The standard pipeline is explicit, not implicit: `Pending → Preloaded → Validated → Executing → Finalizing → Committed`, with `Failed` reachable from any stage. Each state transition goes through the `TransactionState` enum on the `PendingTransaction` entry.

Mechanics:

- **Lock count**: a `PendingTransaction` holds an atomic lock count to prevent premature collection during multi-stage transit — a tx in flight between roles is referenced by more than one stage at a time.
- **Sentinel** drives the early transitions (register → preload → spec validation → deterministic finalizer assignment) and clears entries on `Confirm`/`Reject`.
- **Executor** moves the tx to `Executing`, then `Finalizing` with the assigned finalizer key, once the (stubbed) contract execution returns.
- **Committer gate**: `check_and_commit_transaction_results` accepts `Finalizing` (standard pipeline) or `Validated` (leader-proposal path) — any other state is rejected (see concept-committer-role).
