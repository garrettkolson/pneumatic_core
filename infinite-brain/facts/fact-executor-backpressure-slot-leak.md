---
id: fact-executor-backpressure-slot-leak
title: "Executor backpressure slots are never freed in production"
type: fact
namespace: pneumatic
visibility: namespace
summary: "RESOLVED (Phase 3, 09/28/2026): backpressure slots were never freed in production (only preload_cleanup removed them); slots now live in a separate active_tasks set and are freed by the spawned task on settle."
auto_inject: false
applicable_when: "Historical record of the executor backpressure slot-leak (fixed in Phase 3)"
confidence: 0.3
verified_at: "09/28/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when run_execution (or its completion path) removes the tx from preload_tasks in production code"
tags: [fact, executor, backpressure, defect, capacity]
edges:
  - target: concept-executor-role
    type: related_to
    weight: 0.9
    note: "The leak is in the executor's ingest_preload → run_execution lifecycle"
related: []
source_url: "executor/src/executor.rs:106,137,149"
---

# Executor backpressure slots are never freed in production

Code-verified 09/28/2026.

`Executor::ingest_preload` (`executor/src/executor.rs:149`) registers each tx in the
`preload_tasks` map and `preload_for_transaction` (`:106`) enforces backpressure
against `max_in_flight` (100 in the composite node, `node-server/src/node_server/plugins.rs:70-82`).
The only removal path is `preload_cleanup` (`:137`), which has **zero production
callers** — grep shows it is invoked only from tests. `run_execution`
(`:252-367`) never removes its tx id on completion.

Consequence: after `max_in_flight` transactions, `preload_tasks` is full and the
executor permanently reports `AtCapacity`, rejecting every new preload. Like the
partition-key defect, it is latent in tests (which call `preload_cleanup` manually)
and must be fixed as part of making the executor functional (tracked by
`task-executor-contract-execution`, Phase 3).

**Resolved 09/28/2026 (Phase 3).** The in-flight slot tracking is split from the
results store: `Executor`/`ExecutorHandle` now carry `active_tasks`
(`Arc<Mutex<HashSet<String>>>`) alongside `preload_tasks` (results, persist until
`preload_cleanup`). `is_at_capacity`/`in_flight_count` read `active_tasks`; the
spawned task removes its tx id from `active_tasks` on settle (in
`ExecutorHandle::execute_task`), freeing the slot while leaving the result readable.
`preload_cleanup` removes from both. The `backpressure_slot_freed_after_settle`
dispatch test preloads N sequential txs at `max_in_flight = 1` and asserts none
hit `AtCapacity` — the staleness signal is met.
