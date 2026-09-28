---
id: log-organize-vault-20260928-132816
type: log
operation: organize-vault
date: "2026-09-28T13:28:16"
namespace: pneumatic
summary: "Phase 2 of executor contract execution landed: TransferEngine + SpecEngine interpreter + engine selection"
affected_nodes: ["task-executor-contract-execution", "decision-contract-engine-model"]
tags: ["log", "organize-vault"]
---

Phase 2 of the executor contract-execution plan landed 09/28/2026 in `pneumatic_core::contracts`: `TransferEngine` (amount validation, canonical rmp delta as `result_data`, `TRANSFER_BASE_COST = 21000`) and `SpecEngine` — the versioned rmp ISA interpreter (`LoadTx`, `LoadConst`, checked `Add`/`Sub`/`Mul`/`Mod`, `Cmp`, `Select`, stack-pop `Emit`, `Halt`; gas = instruction count). Per-token selection via `select_engine()` (`contract_engine` metadata key, `Transfer` default for non-contract tokens, fail-closed otherwise). One documented deviation from the plan text: `Emit` pops the stack (plan wrote `Emit(value)`) — an immediate-only emit would leave the arithmetic ops unreachable. Determinism property test: 25 seeded random cases × both engines × 4 runs (plain ×2, tokio current-thread, tokio multi-thread) byte-identical. Workspace `cargo test` green; core suite 558 → 572. ADR-011 node gained the Phase-2 ISA reference.
