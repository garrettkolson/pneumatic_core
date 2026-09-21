---
id: concept-executor-role
title: "Executor role — transaction computation node"
type: concept
namespace: pneumatic
visibility: namespace
summary: "The Executor preloads data, runs backpressure-bounded async execution against the DataProvider, then signs and sends the hashed result to Finalizers as an Execute message."
auto_inject: false
applicable_when: "Working on executor/, transaction execution, backpressure, or the Executor's place in the pipeline"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when executor/src/executor.rs changes its pipeline steps, backpressure API, or outbound action"
tags: [executor, worker-crate, pipeline, backpressure]
edges:
  - target: concept-optimistic-finality
    type: related_to
    weight: 0.9
    note: "The executor's single signed execution output is the input the finalizer's optimistic commit consumes"
  - target: concept-executor-sharding
    type: related_to
    weight: 0.8
    note: "Per-epoch sharding decides which executor serves a transaction; this node is the role itself"
  - target: concept-transaction-lifecycle
    type: related_to
    weight: 0.8
    note: "Drives Preloaded/Executing/Finalizing state transitions in the pending registry"
  - target: pillar-block-lattice
    type: related_to
    weight: 0.7
    note: "Executes transactions that become blocks in the token blockchains"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.7
    note: "Outbound Execute messages are identity-signed Message frames over the wire format"
  - target: fact-workspace-layout
    type: derived_from
    weight: 0.6
    note: "Crate pneumatic_executor is one of the five worker crates around pneumatic_core"
related: []
source_url: "Empty"
---

# Executor role — transaction computation node

The Executor is the transaction-computation stage of the pipeline (`executor/src/executor.rs:23-32`): it receives preloaded transactions, fetches contract/user/token data from the `DataProvider`, executes contract logic, validates results, and sends execution outputs to the assigned Finalizer.

Key mechanics (all code-verified):

- **Backpressure**: a configurable `max_in_flight` caps concurrent executions; `is_at_capacity()` (`executor.rs:84-87`) gates `preload_for_transaction`, which returns `ExecutorError::AtCapacity` at the limit (`executor.rs:106-134`) — new transactions are rejected immediately rather than queued.
- **Execution task**: `preload_for_transaction` spawns an async `execute_task` on a lightweight `ExecutorHandle` clone and tracks the result map in `preload_tasks` (`executor.rs:123-134`, `256-267`).
- **Run pipeline**: `run_execution` loads the tx, transitions it to `Executing`, fetches contract + user data, executes, validates, hashes the output, transitions to `Finalizing` with the finalizer key from the validation result, then broadcasts (`executor.rs:270-367`).
- **Outbound**: `send_to_finalizer` packages `ExecutionResult { transaction_id, result_data, result_hash }` (`executor.rs:464-471`) as a signed `Message` (action `"Execute"`) under the node's own `NodeIdentity` and broadcasts via `node_registry.send_to_all` to `NodeRegistryType::Finalizer` (`executor.rs:146-187`).

The contract-execution body is still a stub — see task-executor-contract-bytecode.
