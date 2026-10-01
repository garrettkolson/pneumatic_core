---
id: task-executor-contract-bytecode
title: "Done (stale): executor contract-execution stub — replaced by real engines (P3, 09/29)"
type: task
namespace: pneumatic
visibility: namespace
summary: "RESOLVED: the execute_contract stub (serialized the tx as its own 'output'; TODO at executor.rs:386) was replaced in plan Phase 3 (09/29) by real ContractEngine dispatch (Transfer/Spec/Wasm). Retained only as the historical staleness marker."
auto_inject: false
applicable_when: "Recalling that the executor's contract-execution stub was replaced, or auditing the closed staleness marker"
confidence: 1.0
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Closed — the stub no longer exists (grep for 'TODO: decode and execute contract bytecode' returns nothing). See task-executor-contract-execution for the landed implementation."
tags: [executor, contract-execution, stub, todo]
edges:
  - target: concept-executor-role
    type: part_of
    weight: 0.9
    note: "The stub sits in the executor's run_execution pipeline (step 5 of 11)"
  - target: concept-optimistic-finality
    type: related_to
    weight: 0.7
    note: "Until this lands, the output that optimistic finality commits is the stubbed tx serialization, not contract state"
  - target: task-executor-contract-execution
    type: followed_by
    weight: 0.8
    note: "Superseded by the full implementation task once the stub is replaced (plan Phase 3)"
related: []
source_url: "Empty"
---

# Open: executor contract execution is a stub (TODO at executor.rs:386)

Genuinely documented pending work in the executor crate — the only TODO found across all five worker crates:

- The doc comment on `execute_contract` (`executor/src/executor.rs:369-377`) states the production intent: decode the contract bytecode/ABI from `contract_data`, run the contract with the transaction as input, return the execution output — and "Currently a stub — returns the transaction body as the 'execution output'."
- The implementation (`executor.rs:377-389`) ignores `contract_data` (`let _ = _contract_data;`) and `// TODO: decode and execute contract bytecode` (`executor.rs:386`) is followed by returning `serialize_to_bytes_rmp(_tx)` — the MsgPack serialization of the transaction itself.
- The downstream pipeline is real and tests it faithfully: the result is validated, SHA-256-hashed, and the optimistic finalizer path commits a block built on this stubbed output, so the stub's semantics (output = tx bytes) are what the finalizer's `build_signed_transaction_optimistic` currently operates on.

Implication: end-to-end pipeline behavior (routing, signatures, quorum, block formation) is fully exercised, but the *computation* itself is identity-like. Landing real bytecode execution here is a semantic change to everything the finalizer commits.

**Status: DONE / STALE** (closed 10/01/2026). The stub was replaced in plan **Phase 3 (09/29/2026)**: `execute_contract` now dispatches to a `ContractEngine` from the `ContractEngineRegistry` (Transfer/Spec defaults, Wasm on demand) under `spawn_blocking` + `catch_unwind`, and the `// TODO: decode and execute contract bytecode` line is gone. This node is retained only as the historical staleness marker; the full implementation (all 10 phases) is tracked by `task-executor-contract-execution`.
