---
id: log-organize-vault-20260928-162624
type: log
operation: organize-vault
date: "2026-09-28T16:26:24"
namespace: pneumatic
summary: "Phase 4 of executor contract execution landed: safety hardening (wall-clock timeout backstop, spawn_blocking + catch_unwind panic isolation, ExecutionTimeout reason) — no unbounded resource path"
affected_nodes: ["task-executor-contract-execution", "decision-contract-engine-model"]
tags: ["log", "organize-vault"]
---

Phase 4 (safety hardening) of the executor contract-execution plan landed 09/28/2026 in `pneumatic_executor` + `pneumatic_core::errors`. The plan's exit criterion — **no unbounded resource path in the executor** — is met on three axes:

- **Wall-clock backstop (Q3.2):** `execute_task` wraps `run_execution` in `tokio::time::timeout` (default **5 s**, env-configurable via `PNEUMATIC_EXECUTOR_TIMEOUT_SECS`, whole seconds; `execution_timeout_from_env()` + `execution_timeout` field + `with_execution_timeout` builder on `Executor`/`ExecutorHandle`). On fire the tx fails with the new `ValidationFailureReason::ExecutionTimeout` (additive, name-tagged serde) and its backpressure slot is freed — no hang, no leaked slot. The timeout arm pulls the tx out of its current (non-terminal) state via `get_transaction_mut` (the `get_transaction` helper only exposes Validated-state txs) and transitions it to `Failed`.
- **Panic isolation:** `execute_contract` (now `async`) runs the engine on a **blocking thread** (`tokio::task::spawn_blocking` — a stuck engine can't block the async worker that checks the timeout) under `std::panic::catch_unwind`; a panicking engine fails the tx with `ContractExecutionFailed` (no new core variant — the existing `Failed` transition in `run_execution` step 5 handles it) and the worker task stays alive.
- **Gas cap** (carried from Phases 2/3): `gas_limit == 0` = no cap, else the engine's `gas_used` is bounded; payload bytes feed the meter.

New `ExecutorError::ExecutionTimeout` unit variant. New `safety` test module (3 tests): sleeping engine → `ExecutionTimeout` + `Failed` (not a hang, slot freed); panicking engine → `ContractExecutionFailed` + no crash; backpressure under timeout load (`max_in_flight = 1`, sequential timed-out preloads never hit `AtCapacity`). **moka LRU deliberately skipped** — it is a performance optimization, not a safety property, and a stale read from a live data service in consensus-critical deterministic execution is dangerous; the rationale is recorded in the task node. Operational limits (max_in_flight, `PNEUMATIC_EXECUTOR_TIMEOUT_SECS` default 5 s, gas cap) documented in `README.md` (new "Executor Operational Limits" subsection); the Roadmap Landed/Outstanding lines updated (real contract execution landed; on-chain deployment = Phase 5 next). Core 572, executor 20, workspace `cargo test` green.
