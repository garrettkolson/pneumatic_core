---
id: decision-contract-engine-model
title: "ADR-011: Pluggable native ContractEngine registry (Wasm deferred to Tier-2)"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Pluggable native ContractEngine trait selected per token by name (spec-registry pattern); Tier-1 ships TransferEngine + SpecEngine (versioned rmp AST); Wasm is a Tier-2 engine, not the substrate."
auto_inject: false
applicable_when: "Choosing, adding, or modifying contract execution engines, the ISA, or per-token engine selection"
confidence: 0.95
verified_at: "09/28/2026"
verified_by: "Garrett Olson"
staleness_signal: "Stale when a Wasm engine lands in Tier-1, the registry selection changes, or engines move out of pneumatic_core"
tags: [adr, design-decision, contract-execution, engine, determinism, is-a]
edges:
  - target: concept-executor-role
    type: related_to
    weight: 0.9
    note: "The engine model is what the executor's execute_contract dispatches to"
  - target: decision-executor-sharding
    type: depends_on
    weight: 0.85
    note: "All shard members must compute identical result_hash, so engines must be pure"
  - target: decision-trait-based-abstraction
    type: derived_from
    weight: 0.8
    note: "Same trait + name-keyed registry + env-spec name list pattern as the validation specs"
  - target: pillar-block-lattice
    type: related_to
    weight: 0.7
    note: "Contract tokens are lattice chains; the engine computes their per-tx state deltas"
  - target: task-executor-contract-bytecode
    type: supports
    weight: 0.8
    note: "Defines the engine the stub replacement dispatches to"
related: []
source_url: "plans/executor-contract-execution-implementation-plan.md (Q1, Q5)"
---

# ADR-011: Pluggable native ContractEngine registry (Wasm deferred to Tier-2)

Approved by Garrett Olson, 09/27/2026.

Contract execution runs on a pluggable native `ContractEngine` trait defined in
`pneumatic_core` (new `src/contracts.rs` module): `fn name() -> &'static str` +
`fn execute(&self, input: &ExecutionInput) -> Result<ExecutionOutput, ContractError>`.
Engines register in a name-keyed `DashMap` registry (ADR-002 pattern) loaded from an
environment-spec list (`contract_engines`), and a token selects its engine via the
metadata key `contract_engine` (mirroring `block_validation_spec_name`; fail closed
when a contract token names an unregistered engine). Tier-1 ships `TransferEngine`
(standard token movement) and `SpecEngine` (interpreter over a versioned, closed,
rmp-serializable instruction AST occupying `SmartContract.bytecode`,
`src/tokens.rs:493-510`). `Call`/snapshot-ref ISA ops are reserved for Model X
cross-contract calls (ADR-014). A `WasmEngine` is a Tier-2 registry entry, not the
substrate.

**Rationale**: determinism is consensus-critical (ADR-008: same-parent conflicts,
same-proposer slashing) — a pure-Rust engine over a closed ISA makes that guarantee
structural, while a Wasm VM would be a large consensus-critical dependency whose
determinism is policy-maintained. Native-first keeps the Wasm option open; Wasm-first
closes it. No Tier-1 need exists for third-party toolchains (the Tier-1 deliverable
is shielded transfer, which bypasses the executor entirely).

**Alternatives rejected**: Wasm VM from day one (wasmi/wasmtime dependency +
determinism audit burden); fixed transfer-only logic (strands the `SmartContract`
model and the `executor.rs:386` TODO).

**Sub-decisions locked**: engines live in `pneumatic_core` (shared by all roles);
per-token selection via token metadata key `contract_engine`.

**Phase-2 ISA reference (implemented 09/28/2026, `src/contracts.rs`)**: bytecode
is the rmp serialization of `InstructionProgram { version: 1, ops: Vec<Op> }`
(version ≠ 1 → `BadBytecode`). Stack machine, all values `u64`; binary ops pop
`a` (top) then `b` and compute `b <op> a`. `LoadTx` fields: `amount`,
`sequence_number`, `gas_limit` (as `u64`); `sender`/`receiver`/`token_id`/
`payload` (first ≤ 8 bytes, left-zero-padded, big-endian). `Add`/`Sub`/`Mul`
checked (overflow/underflow revert), `Mod` (zero divisor reverts), `Cmp`
(pushes 1 if `b > a`, 2 if equal, 3 if `b < a`), `Select` (pops cond, b, a → a if
cond ≠ 0 else b), `Emit` (pops top → 8-byte little-endian `result_data`; last
emit wins), `Halt` (implicit at program end). Gas = instruction count against
`gas_limit` (`0` = no cap; `GasExhausted` → `Failed`, no vote). `TransferEngine`
cost = `TRANSFER_BASE_COST` = 21000.
