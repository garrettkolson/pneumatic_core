---
id: log-organize-vault-20260928-153238
type: log
operation: organize-vault
date: "2026-09-28T15:32:38"
namespace: pneumatic
summary: "Phase 3 of executor contract execution landed: real execute_contract dispatch, real validate_execution_result, D3/D4/D6 defect fixes"
affected_nodes: ["task-executor-contract-execution", "fact-executor-partition-key-defect", "fact-executor-backpressure-slot-leak", "decision-contract-engine-model"]
tags: ["log", "organize-vault"]
---

Phase 3 of the executor contract-execution plan landed 09/28/2026 in `pneumatic_executor`. The `execute_contract` stub is replaced by real dispatch: `run_execution` resolves the token's `SmartContract` asset (fail-closed `ContractNotFound`), selects the engine via `select_engine` (ADR-011), and runs it over `ExecutionInput` carrying the *fetched* sender state (D6). `validate_execution_result` is now a pure module-scope free function: non-empty `result_data`/`result_hash`, `gas_used ≤ gas_limit` when capped, and the `TransferDelta` post-condition (amount/sender/receiver/token_id must match the tx). Defects closed: **D3** — the executor now carries a `partition_id` field (from `env_data.token_partition_id` in `node-server/.../plugins.rs`) and fetches token/user under it; **D4** — backpressure slots are split from results into `active_tasks` (`HashSet`, freed by the spawned task on settle) while `preload_tasks` (results) persists until `preload_cleanup`, so `Err` outcomes are recorded (`Result<ExecutionResult, String>`) and the slot is released; a contract-level failure transitions the tx to `Failed` with the engine's reasons. `ExecutionResult` carries `gas_used`. New tests: 4 dispatch tests (happy-path computed `result_hash` + Sign vote over it, failure→`Failed`, D4 slot-free, D3 partition key) + 5 `validate_execution_result` unit tests. `pipeline_integration` (full wire test) passes with the real dispatch. Core 572, executor 17, workspace `cargo test` green. The two defect facts (`fact-executor-partition-key-defect`, `fact-executor-backpressure-slot-leak`) are resolved.
