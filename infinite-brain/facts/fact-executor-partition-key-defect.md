---
id: fact-executor-partition-key-defect
title: "Executor passes env_id where the token partition id belongs"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Executor::run_execution calls get_data(key, &self.env_id) at executor/src/executor.rs:291,297 while all other roles pass token_partition_id — against a real data service every execution fetch misses."
auto_inject: false
applicable_when: "Debugging executor data fetches, reviewing DataProvider keying, or implementing real contract execution"
confidence: 1.0
verified_at: "09/28/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when executor/src/executor.rs passes env_data.token_partition_id (or the equivalent) to get_data/get_token"
tags: [fact, executor, data-provider, defect, partition-key]
edges:
  - target: concept-executor-role
    type: related_to
    weight: 0.9
    note: "The defect lives in the executor's run_execution data-fetch step"
  - target: concept-data-provider
    type: related_to
    weight: 0.7
    note: "DataProvider keys are (key, partition_id); the executor supplies the wrong partition id"
related: []
source_url: "executor/src/executor.rs:288-298"
---

# Executor passes env_id where the token partition id belongs

Code-verified 09/28/2026.

`Executor::run_execution` fetches contract and user state with
`self.data_provider.get_data(key, &self.env_id)` at `executor/src/executor.rs:291`
and `:297` — passing the **environment id** as the partition key. Every other role
passes the token partition id: the sentinel's processing path uses
`env_data.token_partition_id` (`sentinel/src/sentinel/processing.rs:89`).

Consequence: against a real data service (where data is partitioned by
`token_partition_id`), the executor's fetches are keyed to a partition that does not
exist → `DataError` → every execution fails. The defect is latent in tests because
the test data provider is partition-agnostic. It is independent of the
`execute_contract` stub and must be fixed as part of making the executor functional
(tracked by `task-executor-contract-execution`, Phase 3).
