# Model X Cross-Contract Call Design (ADR-016)

Status: **design for approval** (2026-09-29). Phase 9 of
`plans/executor-contract-execution-implementation-plan.md` ("Cross-contract calls,
Model X (Q6/C4)"). Companion ADR: `infinite-brain/decisions/decision-cross-contract-calls.md`.
Builds on ADR-011 (pluggable `ContractEngine`), ADR-013 (pure read-only executor,
deltas at commit), ADR-014 C4 (Model X: snapshot-pinned, non-atomic), ADR-018
(WasmEngine, W4 `call` host import slot), ADR-008 (determinism is consensus-critical).

The goal: let a contract on token A invoke a contract on token B — **deterministically,
non-atomically, gas-bounded, at bounded depth** — without changing the `ContractEngine`
trait, the executor's dispatch, or per-token engine selection. Both Tier-1 and Tier-2
engines get the capability, expressed per engine: `SpecEngine` via an ISA `Call` op;
`WasmEngine` via the W4 `call` host import. Same semantics, one per-engine surface.

---

## 1. Non-goals (fixed by ADR-014 C4, re-stated)

- **No atomic two-phase commit.** A finalizes on its own chain under optimistic
  finality; B's side is a *separate* cross-referenced tx on B's chain. Withholding A's
  finality until B finalizes would kill first-signature finality for exactly the tx class
  being added (ADR-014 C4, rejected alternative).
- **No cross-token value flow.** A call carries no amount (C6: a contract moves only its
  own token's units; a call is a computation request, not a transfer).
- **No re-architecting the `ContractEngine` trait.** `execute(&self, input)` stays; the
  call capability arrives through `input` (the call context), not a new trait method.

## 2. Core model

### 2.1 The call

`Call(target_token, entry_point, payload, snapshot_ref)` where:

- `target_token` — the B token id (bytes).
- `entry_point` — B's entry action name (string).
- `payload` — calldata for B (bytes).
- `snapshot_ref` — a [`SnapshotRef`]: `{ height: u64, block_hash: Vec<u8> }`, pinning
  B's chain position. `height` is the **0-based block index** in B's chain (genesis = 0).

The executor **resolves B's state at the snapshot ref** and runs B's engine **in the
same call frame** (synchronously, same thread, under a sub-gas budget). B's execution
is a nested, fully deterministic sub-execution: A's `result_hash` is a pure function of
(A program, A input, B state at the ref).

### 2.2 Why the state rides in the input (purity, ADR-013)

Engines are pure functions of `ExecutionInput` — no I/O inside the engine. So the target
state is **supplied to the engine** via a new `ExecutionInput.call_ctx` field
(`Option<Arc<CallContext>>`):

```
CallContext {
    provider: Arc<dyn TargetStateProvider>,  // resolves (target, ref) -> pinned state
    registry: Arc<ContractEngineRegistry>,   // selects B's engine by name
    depth:    u32,                           // 0 at top level; +1 per nested call
}
```

The provider is the *executor's* I/O surface. The engine calls `provider.resolve(...)`
— a function of the data-service state — and the engine remains a pure function of its
input (the provider is part of the input). This is the same pattern as W2/W3: reads are
pinned to the execution-time state snapshot, identical across shard members.

`call_ctx` is **excluded from `canonical_bytes()`** (same rule as `storage`): a contract
that never calls sees byte-identical inputs, so no existing module's `result_hash`
changes (ADR-008).

### 2.3 Snapshot-ref validation (the pin)

`TargetStateProvider::resolve(target_token, ref, sender_key)` — executed by the
executor-side provider, deterministic, **fail-closed**:

1. Fetch B's token (with its `blockchain`) from the data service under the token
   partition.
2. **The ref must be anchored in B's chain**: B's chain must contain a block at index
   `ref.height` whose `current_hash == ref.block_hash`. Any mismatch (height out of
   range, hash mismatch, ref trimmed from B's chain) → the call **fails deterministically**
   (an observable failure, not a hard error — see §3).
3. Decode B's `SmartContract` asset (missing asset → fail).
4. Fetch the sender's `User` state for B's partition (B's engine sees the caller's state
   under B's chain).

**State-sourcing rule (ADR-016 Q1).** The data service is the shared state layer; the
state it reports for a token *is* the state at the ref the provider validates. The ADR
mandates: **the data service serves B's state consistent with the pinned ref; a ref whose
state the service no longer retains is unservable, and a call to it fails closed.** The
pin is a *consensus pin*, not a historical query: for a confirmed ref, every honest node
agrees on the state at that ref, so every A shard member resolves the same state —
**even when B's tip has advanced** between two A executors' reads. That is the pin's
guarantee, and it is what the Phase-9 determinism test pins.

## 3. Failure semantics: deterministic and observable

A call **never kills A**. Every failure mode collapses to a deterministic
`CallOutcome::Failure { reason }` that A's contract logic **observes as data** (a status
value), and A's own execution continues:

| Failure | `CallFailure` |
|---|---|
| Nesting depth cap reached | `DepthExceeded` |
| Target unresolvable (ref not in B's chain, no contract asset, data error) | `TargetUnresolvable` |
| B's engine name not registered | `UnknownEngine` |
| B reverts / bad bytecode / invalid input / gas exhausted | `BReverted` / `BBadBytecode` / `BInvalidInput` / `BGasExhausted` |

**Compensation policy (ADR-016 Q3).** Tier-1 compensation is the calling contract's own
logic: A's result — including any corrective delta A emits — is a pure function of
(A program, A input, call outcome). A dedicated compensation *tx* is a documented
extension, not Tier-1. Contracts **must be written to tolerate partial failure**
(plan risk table: "non-atomic call failure leaves A and B inconsistent" — the
mitigation is exactly this: the failure is deterministic, A settles it in its own
execution, and B's side may simply not land).

## 4. Gas on both chains (ADR-016 Q4)

- **A charges the call:** `CALL_BASE` (per-engine constant) + B's `gas_used` — the
  sub-execution's metered work. A's `gas_used` includes the charge; A's `gas_limit`
  still bounds A as before (exceed → A's standard `GasExhausted`).
- **Sub-budget bounds B:** B runs under `sub = A_remaining − CALL_BASE`. B's own meter
  enforces the sub-budget, so a call can never push A past A's cap.
- **Nesting depth cap:** `MAX_CALL_DEPTH = 8` (the ADR-018 §8 cap-table value). A call
  attempted from a frame at depth 8 fails with `DepthExceeded`. Bounds recursion to a
  fixed, consensus-wide constant.
- Failure charging: a failed call charges A the `CALL_BASE` (the attempt's work) and
  nothing from B. Deterministic by construction.

## 5. B's side: the cross-referenced commitment (ADR-016 Q2)

B's side of a Model X call is a **cross-referenced tx on B's chain** carrying:

```
CrossChainCommitment {
    a_tx_id:       String,   // A's transaction id
    a_result_hash: Vec<u8>,  // A's committed result_hash = hash(A result_data)
    target_token:  Vec<u8>,  // B's token id
    snapshot_ref:  SnapshotRef,
}
commitment_hash = SHA256(b"PNEUMATIC/XCALL/COMMIT/v1" ‖ rmp_canonical(commitment))
```

- **B's sentinel** validates the reference (a pure function `verify_commitment`:
  recompute the commitment hash and compare; check the ref anchors in B's chain).
- **The committer** applies B's state changes **at/after the pinned ref**.
- **Finality independence:** A's finality never depends on B's side. A's execution
  (and its `result_hash`) is computed without any reference to B's post-call blocks;
  B's commitment is settled on B's chain on its own schedule.

The commitment hash is computed **at settlement** (B's side learns A's `result_hash`
from A's committed block) — it is not computed inside A's execution. Phase 9 lands the
schema + the hash function + the pure validator (+ tests); the sentinel/committer B-side
wire is the Phase-10 e2e surface.

## 6. The virtual B transaction (deterministic synthesis)

B's engine needs a canonical `ExecutionInput`. The call synthesizes a **virtual tx**
for B — a pure function of (A tx, call args), identical on every executor:

| field | value |
|---|---|
| `id` | `"xcall/{A_tx_id}"` |
| `action` | `entry_point` |
| `token_id` | `target_token` |
| `bid` | `None` |
| `sequence_number` | A's `sequence_number` (the call happens inside the user's tx) |
| `sender` / `receiver` | A's `sender` / `receiver` (the call is the user's action routed through B) |
| `amount` | `None` (no cross-token value flow, §1) |
| `timestamp` | A's `timestamp` |
| `payload` | the call `payload` |
| `gas_limit` | the sub-budget |
| everything else | zeroed/empty |

B's `sender_state` = the caller's `User` fetched for B's partition (provider step 4).
B's `token` / `contract` / `storage` = the pinned state from the provider.

## 7. Per-engine surfaces (ADR-016 Q5)

**SpecEngine — `Op::Call` (ISA extension, Phase 9).** The call parameters are **static
immediates in the AST** (closed ISA, deterministic by construction):

```
Call { target_token: Vec<u8>, entry_point: String, call_payload: Vec<u8>,
       ref_height: u64, ref_hash: Vec<u8> }
```

The op pushes the **status**: `1` = success, `0` = failure (the `u64` stack cannot
carry B's result bytes — a documented Tier-1 Spec constraint; Wasm gets the bytes).
Adding the variant is additive: existing v1 programs decode unchanged; a `Call` program
on an old node fails closed at decode (Ground Rule 4). The program `version` stays `1`
(format unchanged; ISA extended).

**WasmEngine — `env.call` host import (W4).** Added to `ALLOWED_ENV_IMPORTS` (the
deploy scanner inherits the allow-list automatically):

```
env.call(target_ptr, target_len, entry_ptr, entry_len, payload_ptr, payload_len,
         ref_height: i32, ref_hash_ptr, ref_hash_len, out_ptr, out_cap) -> i32
```

Returns the **result length** written at `out_ptr` (`> 0` = success; `0` = failure).
`ref_height < 0` or any out-of-bounds buffer → deterministic `0`. The import charges
`CALL_BASE + B gas_used` to the module's gas accounting (the W3 `storage_gas` pattern).

## 8. Determinism (ADR-008) checklist

- Engine purity: `call_ctx` is part of the input; no new I/O, clock, or RNG inside
  engines. The provider is a pure function of the data-service state at the ref.
- **The pin test:** two A executors load B at *different tips*; the same pinned ref →
  the same resolved state → identical A `result_hash`.
- No `HashMap` iteration in the call path; the commitment is rmp-canonical with a
  domain prefix; `BTreeMap`/`Vec` only.
- Depth cap, sub-budgets, and all failure reasons are protocol constants / pure
  functions — identical on every node.

## 9. Scope boundary (Phase 9 vs Phase 10)

**Phase 9 (this design):** ADR-016; `call.rs` core (`SnapshotRef`, `TargetStateProvider`,
`PinnedTarget`, `CallContext`, `execute_call`, `CrossChainCommitment` + hash +
`verify_commitment`); Spec `Op::Call`; Wasm `env.call`; executor `CallContext` wiring
(`ExecutorTargetProvider`); the five plan exit tests (snapshot pinning, revert policy,
cross-shard determinism, finality independence, Wasm→Wasm round-trip) + depth/gas/
commitment tests.

**Phase 10 (e2e):** sentinel-side B validation wired into the pipeline; committer apply
at/after the pinned ref; the node-server composite e2e case (e) "a cross-contract call".

## 10. Decision log (Q1–Q5, locked 2026-09-29)

- **Q1 — State sourcing:** the data service serves B's state consistent with the pinned
  ref; the ref must anchor in B's chain (height + hash); unservable ref → deterministic
  call failure. Rejected: historical query API on `DataProvider` (trait-wide change for
  a Tier-1 need); tip-only pinning (fails the pin test — a stale pin would always fail
  and a live pin would be un-pinned).
- **Q2 — B's side:** cross-referenced commitment tx (hash schema above); settled at
  commit on B's chain; A's finality independent. Rejected: two-phase atomic commit
  (ADR-014 C4).
- **Q3 — Compensation:** calling-contract logic is the Tier-1 compensator; deterministic
  failure is data to A. Rejected: protocol-level compensation tx (new subsystem).
- **Q4 — Gas:** A charges `CALL_BASE + B gas_used`; B runs under `A_remaining − CALL_BASE`;
  `MAX_CALL_DEPTH = 8`; failed call charges `CALL_BASE`.
- **Q5 — Surfaces:** Spec `Op::Call` (static AST immediates, status on stack) + Wasm
  `env.call` (bytes out, length return). Same `execute_call` core underneath.
