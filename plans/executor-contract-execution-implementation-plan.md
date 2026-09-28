# Executor Implementation Plan — Real Contract Execution

Status: proposal for review (2026-09-27)
Scope: `pneumatic_executor` crate + minimal `pneumatic_core` substrate. The executor
role's *plumbing* already works end-to-end (verified below); this plan implements the
missing *computation* — real, deterministic contract execution — and fixes the wiring
defects that make the executor non-functional in production.

---

## 1. Research: current state (code-verified)

### 1.1 What already works

- **Pipeline plumbing** (`executor/src/executor.rs`): `ingest_preload` (L149) decodes the
  sentinel's `"Preload"` body (rmp `Transaction`), registers it in the executor's own
  `PendingTransactionRegistry`, and `preload_for_transaction` (L106) enforces
  backpressure (`max_in_flight`) before spawning `execute_task` → `run_execution` (L252):
  load tx → `Executing` → fetch contract + user data → `execute_contract` → SHA-256 the
  output → validate → `Finalizing` (with the assigned finalizer key) → send the finalizer
  an ordered `"Preload"` then a `"Sign"` vote (L435-507).
- **Composite wiring** (`node-server/src/node_server/plugins.rs:70-82`,
  `role_adapters.rs:35-59`): `EXECUTOR_ACTIONS = ["Preload"]`
  (`node_server.rs:39`); the dispatcher routes `Preload` → `Executor::ingest_preload`.
- **Finalizer consumption** (`finalizer/src/finalizer/signing.rs:133-191`,
  `finalizing.rs:161-259`, `block_builder.rs:292-346`): `handle_signature` authenticates
  the voter (envelope auth + registered-Executor gate + inner signature over
  `transaction_hash`), the first valid signature triggers optimistic finality, and
  `build_signed_transaction_optimistic` stamps `transaction.result_hash =
  sig.transaction_hash` into the committed block. The committer's `Executed` block spec
  rejects an empty `result_hash`.
- **Sharding** (ADR-009): the sentinel routes each tx to one per-epoch executor shard
  (`sentinel/src/sentinel/processing.rs:151-195`, `deterministic_select_shard`); all
  shard members execute the same tx and must agree.

### 1.2 What is broken / stubbed (the "not functioning" list)

| # | Defect | Evidence |
|---|--------|----------|
| D1 | **`execute_contract` is an identity stub** — ignores `contract_data`, returns the rmp-serialized tx itself as the "execution output". | `executor/src/executor.rs:370-382` (TODO at :379). Confirmed by vault `task-executor-contract-bytecode` and README:234. |
| D2 | **`validate_execution_result` is a no-op** — only checks the hash is non-empty; no gas, no post-conditions. | `executor/src/executor.rs:400-416`. |
| D3 | **Wrong data partition key** — the executor calls `get_data(key, &self.env_id)` (L291, L297), i.e. passes the *environment id* where every other role passes the *token partition id* (`sentinel/processing.rs:89` uses `env_data.token_partition_id`). Against a real data service the fetch is keyed to a non-existent partition → `DataError` → **every execution fails**. | `executor/src/executor.rs:288-298`. |
| D4 | **Backpressure slots are never freed** — `preload_cleanup` (L137) is called only from tests; production `run_execution` never removes the `tx_id` from `preload_tasks`. After `max_in_flight` txs the executor is permanently `AtCapacity` and rejects everything. | grep: `preload_cleanup` has zero production callers. |
| D5 | **Protocol gap: `Transaction` has no calldata field** — only `action: String`, `amount: Option<u64>`, sender/receiver (`src/transactions.rs:162-192`). Real contract calls have nowhere to carry input data. | `src/transactions.rs`; no `payload`/`data` field. |
| D6 | **No execution bounds** — no gas metering, no wall-clock timeout, no panic isolation; fetched user data is fetched and discarded (`_user_data`, L295); execution tasks are fire-and-forget (`tokio::spawn`, L243) and nothing observes failures. | `executor/src/executor.rs:238-249, 294-298`. |

### 1.3 Protocol constraints that shape the design

1. **Optimistic finality ⇒ determinism is consensus-critical.** The first valid executor
   signature finalizes immediately (ADR-005/010); a conflict is two valid blocks with the
   same `(token_id, previous_hash)` and different `current_hash` (ADR-008) — and
   same-proposer double-signing is slashed. Every executor in a shard must compute the
   **same** `result_hash` for the same tx, so execution must be a *pure function* of
   canonical inputs: no wall clock, no RNG, no I/O, no host-dependent behavior inside the
   engine.
2. **Contract model already exists in core.** `Token.asset_data` carries the rmp-
   serialized asset — for contract tokens a `SmartContract { name, bytecode: Vec<u8>,
   metadata }` (`src/tokens.rs:493-510`), minted via `TokenFactory::mint_contract_token`
   (metadata `token_type = "contract"`). The executor's `contract_data` fetch
   (L289-292) is exactly this blob. `ProxyAuthorization` (contract proxy access) also
   exists and is unused.
3. **State model.** Global `User { public_key, fuel_balance, stake, nonce }`
   (`src/user.rs:6-15`) + per-token `Account { public_key, balance }` stored in
   `Token.asset_data`. All state lives in the external data service behind
   `DataProvider`; the executor is stateless today (read-only fetches).
4. **Spec-registry pattern is the house style.** `TransactionValidationSpec` /
   `BlockValidatorSpec` traits + name-keyed registries + environment-spec name lists
   (`trans_validation_specs`, `block_validation_specs`); per-token `BlockValidator`
   trait. A `ContractEngine` trait selected by name fits this pattern exactly.
5. **Additive wire-change precedent.** The shielded `SignedTransaction.shielded` field
   (pneumatic-shielded-implementation-plan.md Ground Rule 4) shows the accepted pattern
   for evolving the wire: additive field, `#[serde(default)]`, canonical bytes of
   unaffected structures stay byte-identical.

---

## 2. Design decisions (P0 — approve before coding)

Each decision lists the recommendation and the rejected alternative.

### Q1 — Engine model (the core decision)

**Recommended: pluggable native `ContractEngine` trait + name-keyed registry** (the
spec-registry pattern). Engines are pure Rust implementations; the environment spec
names which engines are loaded; per-token selection via token metadata
(`token_type` / a new `contract_engine` key) — mirroring `block_validation_spec_name`.

- Tier-1 engines (this plan): `TransferEngine` (standard token movement, deterministic
  output) and `SpecEngine` (interpreter over a documented, rmp-serializable instruction
  format that occupies `SmartContract.bytecode`).
- Tier-2 (separate plan, P6 here): a `WasmEngine` (wasmi/wasmtime) implementing the same
  trait, for true third-party bytecode.

*Alternatives rejected:* (a) Wasm VM from day one — new heavy dependency, supply-chain
and determinism audit burden, and it solves a Tier-2 need; (b) fixed
transfer-only logic — cannot honor the existing `SmartContract`/`bytecode` model or the
"decode and execute contract bytecode" TODO.

### Q2 — Calldata

**Recommended: add `payload: Vec<u8>` to `Transaction`**, additive with
`#[serde(default)]` (Ground Rule 4). Non-contract txs keep `payload = vec![]` and every
canonical byte sequence derived from existing tx shapes stays unchanged. Contract calls
carry their input in `payload`; `action` names the entry point (e.g. `"Transfer"`,
`"ContractCall"`); `amount` is the value moved.

*Alternative rejected:* encoding calldata in `action`/`amount` — unbounded, lossy, and
breaks the canonical-transaction signing model's clarity.

### Q3 — Gas / resource bounds

**Recommended: two bounds, both enforced before a result is accepted.**
1. *Instruction budget*: the engine counts instructions/steps against a per-tx cap
   (default derived from `EnvironmentMetadata.max_gas_limit`; the tx's `bid`/`amount`
   can lower it). Out-of-budget = failed execution → `Failed` state, no vote emitted.
2. *Wall-clock timeout*: `tokio::timeout` around `execute_task` (default 5 s) as a
   defense-in-depth backstop; timeout = failed execution.

Fuel accounting against `User.fuel_balance` at the data service is a data-service
concern (see Q4); the executor only *enforces* bounds.

### Q4 — State semantics

**Recommended: the executor stays a pure, read-only function.**
`result = f(canonical_tx, token_asset_state, user_state)`. The canonical output encodes
the *intended state delta* (e.g. `(sender, receiver, amount, token)` for a transfer);
applying the delta to `User`/`Account` balances remains the data service's job at
commit time (the committer is already the role that persists state —
`save_token`/`save_shielded_pool`). This keeps sharding safe (no write ordering
questions between shard members) and keeps the executor horizontally scalable.

*Alternative rejected:* executor writes state via `save_user`/`save_data` — introduces
write-ordering races between shard members and couples execution to the data service's
write path.

### Q5 — Bytecode format for `SpecEngine`

**Recommended: a documented rmp-serializable instruction AST** (a small, closed set:
load field, binary ops, compare, select, halt/output) defined in `pneumatic_core` so
all roles can parse/verify it. It occupies `SmartContract.bytecode` (rmp-encoded) and
is fully deterministic by construction. The format is documented as an ADR and versioned
(first byte = format version).

*Alternative:* Wasm now — see Q1.

---

## 3. Phased implementation

Each phase is independently buildable, testable, and mergeable on top of the last.

### Phase 0 — Lock the decisions (no code)

- Approve Q1–Q5 above (or amend).
- Write the ADR into the vault: one `decision-*` node for the engine model (Q1/Q5), one
  for calldata (Q2), one for state semantics (Q4); update
  `task-executor-contract-bytecode` → status "planned"; add a task node for this plan.
- Record the D3/D4 wiring defects as `fact` nodes (they are independent bugs that ship
  with this work).

**Exit:** decisions approved; vault nodes exist.

### Phase 1 — Core execution substrate (`pneumatic_core`)

New module `src/contracts.rs` (declared in `src/lib.rs`):

- `ContractEngine` trait (async, `Send + Sync`):
  `fn name() -> &'static str;`
  `fn execute(&self, input: &ExecutionInput) -> Result<ExecutionOutput, ContractError>;`
- `ExecutionInput` — the *canonical* input, one struct so every engine and every test
  hashes the same bytes: `{ tx: &Transaction (incl. new `payload`), contract:
  &SmartContract (decoded from `Token.asset_data`), sender_state: &User,
  token: &Token, gas_limit: u64 }`.
- `ExecutionOutput { result_data: Vec<u8>, gas_used: u64 }` — `result_data` is
  rmp-canonical; `result_hash` remains `hash(result_data)` (no new hashing).
- `ContractError` enum (UnknownEngine, GasExhausted, BadBytecode, Reverted, …) with a
  mapping to `ValidationFailureReason` for the `Failed` transition.
- Engine registry: `DashMap<String, Arc<dyn ContractEngine>>` (ADR-002 pattern) +
  registration of the built-ins; environment-spec wiring: `contract_engines:
  Vec<String>` in `EnvironmentMetadataSpec` (default = both built-ins), loaded in
  `EnvironmentMetadata::load_from_spec`.
- **Also in this phase (protocol change, Q2):** add `#[serde(default)] pub payload:
  Vec<u8>` to `Transaction` (`src/transactions.rs:162`), update `CanonicalTransaction`
  (the signed canonical form, `verify_sender_signature`) to include `payload` — the
  sender must sign calldata — and update every `Transaction` literal in the workspace
  (builders/tests). Additive: `#[serde(default)]` keeps legacy rmp valid; add a
  regression test proving the rmp bytes of a tx with empty `payload` are unchanged.

Tests (core): canonical-input determinism (two `ExecutionInput`s from the same source
data → identical rmp bytes); empty-payload wire regression; registry lookup/unknown.

**Exit:** `cargo test` green; substrate unit-tested; wire regression proven.

### Phase 2 — Built-in engines

In `pneumatic_core::contracts` (engines live in core so all roles — and future
`WasmEngine` — share them):

- `TransferEngine` (`name() = "Transfer"`): validates `amount` (overflow-safe),
  encodes the canonical delta `(token_id, sender, receiver, amount, sequence_number)`
  as `result_data`. This is the standard-token semantics and the replacement for the
  identity stub on non-contract txs.
- `SpecEngine` (`name() = "Spec"`): decodes `SmartContract.bytecode` as the versioned
  rmp instruction AST (Q5), interprets it against `ExecutionInput` fields with an
  instruction counter (gas, Q3). Small closed ISA (documented in the ADR):
  `LoadTx(field)`, `LoadConst(n)`, `Add/Sub/Mul/Mod` (overflow = revert), `Cmp`,
  `Select`, `Emit(value)` (sets `result_data`), `Halt`. Unknown/undeclared op = revert.
- Per-token engine selection: token metadata key `contract_engine` (default
  `"Transfer"` when `token_type != "contract"`; contract tokens must name a registered
  engine — fail closed otherwise).

Tests: each engine's happy path; gas exhaustion; malformed bytecode fails closed;
**determinism property tests** — N random tx/contract inputs executed twice (and under
`tokio` current-thread vs multi-thread) must yield byte-identical `result_data`.

**Exit:** engines pass determinism property tests; core test count up.

### Phase 3 — Replace the stub + fix the wiring defects (executor crate)

In `executor/src/executor.rs`:

- `execute_contract` (L370-382) → real dispatch: decode `contract_data` as the token
  asset → `SmartContract`/`Token`, select the engine (Phase 2 rule), build
  `ExecutionInput` (using the *fetched* user data, D6), call the engine, return
  `result_data`. Remove the TODO.
- `validate_execution_result` (L400-416) → real checks: non-empty `result_data`,
  `gas_used ≤ limit`, engine-specific post-conditions (e.g. transfer amount
  consistency), map `ContractError` → `ValidationFailureReason`.
- **Fix D3:** `run_execution` takes the partition id from the environment
  (`token_partition_id`), not `env_id` — plumb it into `Executor::new` (the
  node-server passes `env_data.token_partition_id`; see `plugins.rs:114-139`
  precedent). Add `ExecutorHandle` field.
- **Fix D4 (slot leak):** free the backpressure slot on task completion — move the
  `preload_tasks` removal *into* `execute_task` (after `run_execution` settles, success
  or failure), keeping `preload_cleanup` as an idempotent public API for tests. Add a
  test: N sequential executions with `max_in_flight = 1` never reach `AtCapacity`.
- Make execution failures observable: `execute_task` records `Err` variants into the
  results map (today only `Ok` is inserted, L245-247) and logs with `log::warn!`.

Tests (executor): dispatch-to-engine happy path (recorded connection captures a
`"Sign"` vote whose `transaction_hash` = SHA-256 of the *engine* output, not the tx
bytes); failure → `Failed` transition with reasons; slot-leak regression; partition-id
regression (fetch uses the token partition key).

**Exit:** the stub is gone; a contract tx's block carries a computed `result_hash`;
all D1–D4, D6 defects closed; `cargo test` green.

### Phase 4 — Safety hardening

- `tokio::timeout` (default 5 s, env-configurable) around `execute_task` (Q3.2).
- `std::panic::catch_unwind` around the engine call so a buggy engine fails the tx
  instead of killing the worker task.
- Optional: moka LRU (already a core dependency, `src/data.rs:4`) for
  contract/user-asset fetches within an execution to cut data-service round trips.
- Document the operational limits (max_in_flight, timeout, gas cap) in the README.

Tests: timeout → failed execution, not hang; panic isolation; backpressure under
timeout load.

**Exit:** no unbounded resource path in the executor; tests green.

### Phase 5 — End-to-end verification + docs

- **node-server composite e2e** (extend `node-server/src/node_server/tests/e2e.rs`):
  full pipeline `Sentinel Verify → Executor Preload → Finalizer Sign → Committer
  Commit` for (a) a standard transfer tx and (b) a `SpecEngine` contract tx, asserting
  the committed block's `result_hash` equals an independently computed
  `hash(engine_output)`. This is the test that proves the executor "functions".
- **Cross-executor determinism test** (two `Executor` instances, same shard inputs →
  identical `result_hash` and identical `"Sign"` votes) — the sharding invariant.
- Update `TASKS.md` (executor tail), README (Outstanding section), and the vault:
  `task-executor-contract-bytecode` → done/stale per its staleness signal; update
  `concept-executor-role` (pipeline steps changed), add event node; bump
  `roadmap-phase-status`.

**Exit:** e2e green; docs + vault current.

### Phase 6 — (Optional, Tier-2) Wasm engine

A `WasmEngine` implementing `ContractEngine` (wasmi, no-std-friendly, deterministic
feature set only) so third-party `SmartContract.bytecode` can be Wasm. Separate plan;
blocked on nothing in Phases 1–5.

---

## 4. Risks & mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Any engine nondeterminism (HashMap iteration, float, platform) | Same-parent conflicts, same-proposer slashing (ADR-008) | Phase-2 determinism property tests are a merge gate; engines use only BTreeMap/Vec canonical rmp; no floats in ISA |
| `payload` wire change breaks canonical-transaction signing | Rejected valid txs / replay issues | Ground Rule 4 additive pattern + `#[serde(default)]` + byte-identical regression test; `payload` included in `CanonicalTransaction` so it is covered by the sender signature |
| `result_hash` semantics change what blocks commit | All downstream block hashes change (intended) | This is the point of the work; e2e test pins the new semantics |
| D3 partition fix changes what the executor can see | Executions that "worked" on mis-keyed data (they didn't — D3 makes them fail) | D3 is a fix, not a behavior change, against any real data service |
| Engine bugs (overflow, gas accounting) | Failed txs, wasted gas | Fails closed to `Failed` state; no vote emitted on any engine error |
| Scope creep toward a full EVM | Slip | Tier-1 ISA is closed and small; Wasm is explicitly Phase 6 |

## 5. Effort estimate (rough)

- P0: decisions only.
- P1: 1–2 days (core module + wire field + literals).
- P2: 2–3 days (two engines + ISA doc + property tests).
- P3: 1–2 days (dispatch + D3/D4 fixes + executor tests).
- P4: 0.5–1 day.
- P5: 1 day (e2e + docs + vault).
- P6: separate estimate.

## 6. Standing vault duties (per CLAUDE.md)

Update on landing: `task-executor-contract-bytecode` (close), `concept-executor-role`
(pipeline steps), `decision-*` new nodes (P0), `fact-*` nodes for D3/D4,
`roadmap-phase-status`, `_system/INDEX.md`, one log node per significant landing.
