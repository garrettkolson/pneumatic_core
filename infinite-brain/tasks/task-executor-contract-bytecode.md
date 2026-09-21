---
id: task-executor-contract-bytecode
title: "Open: executor contract execution is a stub (TODO at executor.rs:386)"
type: task
namespace: pneumatic
visibility: namespace
summary: "Executor::execute_contract is a documented stub that serializes the tx itself as the 'output'; TODO at executor/src/executor.rs:386: decode and execute real contract bytecode."
auto_inject: false
applicable_when: "Planning real contract execution, reviewing what the executor actually computes today, or assessing optimism-path risk"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Done/stale when execute_contract performs real bytecode decoding/execution — grep executor/src/executor.rs for 'TODO: decode and execute contract bytecode'"
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
related: []
source_url: "Empty"
---

# Open: executor contract execution is a stub (TODO at executor.rs:386)

Genuinely documented pending work in the executor crate — the only TODO found across all five worker crates:

- The doc comment on `execute_contract` (`executor/src/executor.rs:369-377`) states the production intent: decode the contract bytecode/ABI from `contract_data`, run the contract with the transaction as input, return the execution output — and "Currently a stub — returns the transaction body as the 'execution output'."
- The implementation (`executor.rs:377-389`) ignores `contract_data` (`let _ = _contract_data;`) and `// TODO: decode and execute contract bytecode` (`executor.rs:386`) is followed by returning `serialize_to_bytes_rmp(_tx)` — the MsgPack serialization of the transaction itself.
- The downstream pipeline is real and tests it faithfully: the result is validated, SHA-256-hashed, and the optimistic finalizer path commits a block built on this stubbed output, so the stub's semantics (output = tx bytes) are what the finalizer's `build_signed_transaction_optimistic` currently operates on.

Implication: end-to-end pipeline behavior (routing, signatures, quorum, block formation) is fully exercised, but the *computation* itself is identity-like. Landing real bytecode execution here is a semantic change to everything the finalizer commits.
