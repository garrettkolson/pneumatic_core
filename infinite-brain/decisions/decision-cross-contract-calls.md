---
id: decision-cross-contract-calls
title: "ADR-016: Model X cross-contract calls (snapshot-pinned, non-atomic, gas-bounded)"
type: decision
namespace: pneumatic
visibility: namespace
summary: "A contract on token A invokes a contract on token B via Call(target, entry, payload, snapshot_ref): the executor resolves B's state pinned at the ref (fail-closed) and runs B's engine in the same frame under a sub-gas budget; B's side is a cross-referenced commitment, A's finality independent. Spec Op::Call + Wasm env.call share one execute_call core."
auto_inject: false
applicable_when: "Designing, scoping, or implementing cross-contract calls (Model X), snapshot-pinned reads, call gas/depth, or the B-side commitment"
confidence: 0.95
verified_at: "09/30/2026"
verified_by: "dsh-agent"
staleness_signal: "Phase 9 (core) landed 09/30/2026. Stale when the P10 B-side wire/committer apply lands and changes the commitment settlement, or when a deeper/atomic call model supersedes the non-atomic C4 rule"
tags: [adr, design-decision, contract-execution, model-x, cross-contract-call, snapshot-pin, gas, determinism, wasm, spec]
edges:
  - target: decision-contract-model-lifecycle
    type: derived_from
    weight: 0.95
    note: "Implements ADR-014 C4 (Model X): snapshot-pinned, non-atomic, M-of-N-independent"
  - target: decision-contract-engine-model
    type: depends_on
    weight: 0.9
    note: "No new ContractEngine method — the capability arrives per engine (Spec Op::Call, Wasm env.call) over the existing execute(input) trait"
  - target: decision-executor-pure-read-only
    type: depends_on
    weight: 0.9
    note: "B's pinned state rides in ExecutionInput.call_ctx, excluded from canonical_bytes() (same rule as storage) — the engine stays a pure function of its input"
  - target: decision-wasm-engine-tier2
    type: supports
    weight: 0.85
    note: "W4 (the Wasm form of Model X) lands as the env.call host import on the ADR-018 WasmEngine"
  - target: task-executor-contract-execution
    type: supports
    weight: 0.9
    note: "ADR-016 is Phase 9 of the executor contract-execution plan"
  - target: fact-wasmparser-read-var-i32-bug
    type: related_to
    weight: 0.6
    note: "The Wasm->Wasm round-trip fixture requires the sleb_force2 workaround for the wasmparser 0.239.0 const-decode bug"
related: ["[[plans/model-x-call-design.md]]"]
source_url: "plans/model-x-call-design.md"
---

# ADR-016: Model X cross-contract calls (snapshot-pinned, non-atomic, gas-bounded)

**Phase 9 landed 2026-09-30.** Full design: `plans/model-x-call-design.md`. A contract on
token A invokes a contract on token B **deterministically, non-atomically, gas-bounded, at
bounded depth** — without changing the `ContractEngine` trait, the executor's dispatch, or
per-token engine selection. Both Tier-1 and Tier-2 engines get the capability, expressed per
engine: `SpecEngine` via an ISA `Op::Call`; `WasmEngine` via the W4 `env.call` host import.
Same semantics, one `execute_call` core underneath.

**Core model**
- **The call:** `Call(target_token, entry_point, payload, snapshot_ref)` where
  `SnapshotRef { height: u64, block_hash: Vec<u8> }` pins B's chain position (0-based block
  index, genesis = 0). The executor resolves B's state at the ref and runs B's engine **in the
  same call frame** (synchronously, same thread, under a sub-gas budget) — a nested, fully
  deterministic sub-execution. A's `result_hash` is a pure function of (A program, A input, B
  state at the ref).
- **Purity (ADR-013):** the target state is supplied to the engine via a new
  `ExecutionInput.call_ctx: Option<Arc<CallContext>>` — `CallContext { provider:
  Arc<dyn TargetStateProvider>, registry: Arc<ContractEngineRegistry>, depth: u32 }`. The
  provider is the *executor's* I/O surface; the engine calls `provider.resolve(...)` and stays
  a pure function of its input. `call_ctx` is **excluded from `canonical_bytes()`** (same rule
  as `storage`): a contract that never calls sees byte-identical inputs, so no existing module's
  `result_hash` changes (ADR-008).
- **Snapshot-ref validation (the pin):** `TargetStateProvider::resolve(target_token, ref,
  sender_key)` is deterministic and **fail-closed** — fetch B's token (with its `blockchain`),
  validate the ref (block exists at `height` AND its `current_hash == ref.block_hash`), decode
  the `SmartContract` asset, fetch the sender user. Any miss → `ContractError::InvalidInput`
  (a deterministic call failure, not a trap).
- **Gas (Q4):** A charges `CALL_BASE + B.gas_used`; B runs under the sub-budget
  `A_remaining − CALL_BASE`; `MAX_CALL_DEPTH = 8`; a failed call still charges `CALL_BASE`.
  Constants: `XCALL_CALL_BASE_SPEC = 10`, `XCALL_CALL_BASE_WASM = 100`, domain
  `b"PNEUMATIC/XCALL/COMMIT/v1"`.
- **Surfaces (Q5):** `SpecEngine` gains `Op::Call { target_token, entry_point, call_payload,
  ref_height, ref_hash }` (static AST immediates; pushes status 1 = success / 0 = failure onto
  the stack). `WasmEngine` gains `env.call(target_ptr,len, entry_ptr,len, payload_ptr,len,
  ref_height, ref_hash_ptr,len, out_ptr,out_cap) -> i32` = result length (**0 on any failure —
  never traps**); in `ALLOWED_ENV_IMPORTS`; `call_gas` folded into `gas_used`.
- **B's side (Q2):** a **cross-referenced commitment** tx on B's chain,
  `commitment = H(A_tx_id ‖ A_result_hash ‖ snapshot_ref)`, settled at commit on B's chain.
  **A's finality is independent** — no atomic two-phase commit (ADR-014 C4, rejected
  alternative). The virtual B tx: `id = "xcall/{A_tx_id}/{target_hex}"`, `action =
  entry_point`, `token_id = target`, `payload = call_payload`, `gas_limit = sub_budget`,
  `amount = None` (no cross-token value flow — a call is a computation request, not a transfer).
- **Compensation (Q3):** the calling-contract logic is the Tier-1 compensator; a deterministic
  call failure is **data to A** (status 0), not a protocol-level compensation tx.

**Landed (2026-09-30)** in `pneumatic_core::contracts::call` (new `src/contracts/call.rs`:
`SnapshotRef`, `TargetStateProvider`, `CallContext`, `PinnedTarget`, `CrossChainCommitment`,
`CallOutcome`, `CallFailure`, `validate_snapshot_ref`, `synthesize_call_tx`, `execute_call`),
`SpecEngine` `Op::Call` interpreter branch (`src/contracts.rs`), the `WasmEngine` `env.call`
host import (`src/contracts/wasm.rs`), and the executor wiring (`executor/src/executor.rs`:
private `ExecutorTargetProvider` impl + `call_ctx` built in `execute_contract`). 13 core call
tests + 2 Wasm→Wasm round-trip tests + 7 executor tests (6 provider fail-closed + 1 full
wiring). Core 654, executor 31, workspace `cargo test` green (0 failed). **P10** (sentinel
B-side wire, committer apply, node-server e2e) remains.
