# Executor Implementation Plan — Real Contract Execution

Status: **Phase 2 complete** (2026-09-28) — decisions Q1–Q6 approved
(2026-09-27/28, Garrett Olson); ADR-011–014 recorded in the vault; Phase 1 landed
(`src/contracts.rs` substrate + `payload`/`gas_limit` wire fields); Phase 2 landed
(`TransferEngine` + `SpecEngine` interpreter, per-token engine selection,
determinism property tests — workspace tests green, core suite 572); Phase 3 next
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
   exists and is unused. The lifecycle policy for this model is now an explicit
   decision (Q6 below): contract-as-token, on-chain deployment, Model X
   cross-contract calls, multisig+timelock upgrades.
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

**Approved 2026-09-27.** Sub-decisions locked: engines live in `pneumatic_core`
(shared by all roles); per-token selection via the token metadata key
`contract_engine`.

### Q2 — Calldata

**Recommended: add `payload: Vec<u8>` to `Transaction`**, additive with
`#[serde(default, skip_serializing_if = "Vec::is_empty")]` (Ground Rule 4 — the
skip-if-empty is what keeps every existing tx byte-identical on the wire). Contract
calls carry their input in `payload`; `action` names the entry point (e.g.
`"Transfer"`, `"ContractCall"`); `amount` is the value moved. Two refinements:
`payload` joins `CanonicalTransaction` (the sender's signature must cover calldata —
without it a relay can swap calldata on a signed tx), and a sentinel-enforced size cap
(env-configurable, a few KB) bounds the DoS surface and feeds gas (Q3). Client
convention: `tx.id` derived from the canonical form *including* payload, so
calldata-differing txs never collide in the registry.

*Alternatives rejected:* encoding calldata in `action`/`amount` (overloads a routing
key, unbounded, lossy); sidecar calldata outside the tx (not covered by the sender
signature — malleability hole); deferring the field until `SpecEngine` needs it
(same exercise done twice).

**Approved 2026-09-27.**

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

*Alternative:* Wasm now — see Q1. Note the ISA is designed to accommodate Model X
cross-contract calls (Q6/C4): `Call` and snapshot-ref instructions are reserved in the
format, implemented in Phase 7.

### Q6 — Contract model & lifecycle

**Approved 2026-09-27** (contract-as-token confirmed; on-chain deployment,
cross-contract calls, and upgrade governance promoted from "future" to in-scope):

- **C1 — Ontology: contract ≡ contract token (1:1).** One contract = one token = one
  chain in the block lattice; contract state = the token's `asset_data` (bytecode) +
  that token's account state. *Rejected:* many contracts inside one token (EVM-style
  account model) — the pipeline is partitioned per-token end to end.
- **C2 — On-chain deployment.** A `DeployContract` tx action creates a contract token
  on-chain (no admin step). The token id is deterministic and derivable before
  execution: `token_id = H(deployer_pubkey ‖ deployer_nonce ‖ bytecode_hash ‖ name)`
  (`User.nonce` provides replay protection). Deployment is a protocol op, not contract
  logic: the executor emits a `CreateToken` state delta (Q4 model) that the data
  service applies at commit. *Rejected:* admin-gated off-chain genesis.
- **C3 — Public execution.** Anyone may send a tx to any registered contract token
  (`token_id` + entry-point `action` + `payload`); effects are constrained by C6.
  `ProxyAuthorization` remains the (future) inter-contract authorization primitive.
- **C4 — Cross-contract calls: Model X (snapshot-pinned, non-atomic).** A call from
  contract A (token T_A) to contract B (token T_B) carries an explicit snapshot ref
  `B@(height, hash)`; the executor reads B's state at that pinned ref (deterministic
  across A's shard members); B's side is a cross-referenced tx on B's chain validated
  against the same ref. A finalizes on its own chain under optimistic finality; if
  B's side fails, a deterministic revert/compensation tx on A's chain settles it.
  *Rejected:* two-phase atomic commit — it withholds A's finality until B's finalizes,
  killing optimistic finality for exactly the tx class being added.
- **C5 — Upgrades: multisig + timelock.** A contract registers N owner keys; an
  upgrade tx (new bytecode → new `asset_data`/`asset_hash`) requires M-of-N owner
  signatures plus a mandatory delay (one epoch) before it applies. Stake-weighted
  voting is a documented future extension, not a Tier-1 requirement. *Rejected:*
  admin-gated upgrades; full on-chain voting (a new protocol subsystem).
- **C6 — Effect scope.** A contract may only move units of *its own* token: state
  deltas reference that token's accounts, no negative balances (checked at apply
  time), fuel (`User.fuel_balance`) is untouched.

Follow-on design ADRs (written at the start of each phase, full design):
**ADR-015** on-chain deployment mechanics, **ADR-016** cross-chain call semantics
(Model X full design: snapshot-ref validation, revert policy, gas on both chains),
**ADR-017** upgrade governance (owner registry, M-of-N rules, timelock parameters).

## 3. Phased implementation

Each phase is independently buildable, testable, and mergeable on top of the last.

### Phase 0 — Lock the decisions (no code)

- Approve Q1–Q6 (status at 2026-09-27: **Q1, Q2, Q4, Q6 approved; Q3, Q5 pending**).
- Write the ADRs into the vault: `decision-contract-engine-model` (ADR-011, Q1/Q5),
  `decision-tx-calldata-payload` (ADR-012, Q2), `decision-executor-pure-read-only`
  (ADR-013, Q3/Q4), `decision-contract-model-lifecycle` (ADR-014, Q6); update
  `task-executor-contract-bytecode` → status "planned"; add a task node for this plan.
- Record the D3/D4 wiring defects as `fact` nodes (they are independent bugs that ship
  with this work).
- Execution detail: `plans/executor-p0-decisions-implementation-plan.md`.

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

### Phase 5 — On-chain contract deployment (Q6/C2)

Design ADR first: **ADR-015 — on-chain deployment mechanics** (deterministic token-id
derivation, bytecode size/format limits, `CreateToken` delta schema, committer →
data-service apply path, shard selection for a not-yet-existing token id).

Implementation:

- `DeployContract` tx action through the standard pipeline; a sentinel-side
  `DeployValidationSpec` (bytecode size cap, engine name registered, name rules)
  fails closed before routing.
- `DeployEngine` (a protocol op, not contract logic): validates the deployment,
  computes the deterministic `token_id`, emits `CreateToken { token, contract,
  initial_state }` as the canonical delta (Q4) — the executor still writes nothing.
- Data-service apply path at commit: new token record in the token partition
  (`save_token`), idempotent (re-apply of the same delta is a no-op).
- Gas: deployment cost = base + f(bytecode size).

Tests: deterministic id (same inputs → same id; replay rejected by nonce); deploy
e2e through the composite (new token exists in the data service after commit and its
contract txs then execute); idempotent re-apply.

**Exit:** a contract can be created by a tx, not by an admin; e2e green.

### Phase 6 — Upgrade governance (Q6/C5)

Design ADR first: **ADR-017 — upgrade governance** (owner registry schema, M-of-N
rules, timelock parameters, interaction with deployment).

Implementation:

- Owner registry in contract metadata (`owners: Vec<PublicKey>`, `threshold: u32`),
  settable at deployment; an `UpgradeContract` tx action carries new bytecode +
  owner-set changes (owner-set changes themselves require M-of-N).
- Upgrade = a `ReplaceAsset` delta (new `asset_data`, new `asset_hash`); the timelock
  is enforced at the data service at apply time: an upgrade delta only applies at or
  after proposal epoch + 1 (epoch data from the committer).
- The sentinel spec validates signature count/quorum; the executor re-validates
  deterministically (same check, same inputs) so the vote is in the canonical output.

Tests: quorum edges (M-1 sigs rejected, M accepted); timelock (early apply rejected);
owner-set rotation; a successful upgrade changes the engine behavior of subsequent txs.

**Exit:** upgrades are a governed on-chain act; e2e green.

### Phase 7 — Cross-contract calls, Model X (Q6/C4)

The largest new protocol surface. Design ADR first: **ADR-016 — cross-chain call
semantics** (snapshot-ref validation, cross-referenced tx schema, deterministic
revert/compensation policy, gas on both chains, interaction with optimistic finality).

Design constraints fixed by Q6: calls are **non-atomic** — A finalizes on its own
chain under optimistic finality; B's side is a separate tx on B's chain; failure on
B's side settles via a deterministic compensation on A's chain.

Implementation:

- ISA extension: `Call(target_token, entry_point, payload, snapshot_ref)` — the
  executor resolves B's state at `snapshot_ref` (height + block hash, validated
  against B's chain) and executes B's engine in the same call frame under a
  sub-budget.
- B's side: a cross-referenced tx on B's chain carrying `commitment =
  H(A_tx_id ‖ A_result_hash ‖ snapshot_ref)`; B's sentinel validates the reference;
  the committer applies B's state changes at/after the pinned ref.
- Failure: if B's side reverts/fails, A's contract logic observes the deterministic
  failure result (a call returning an error); an optional compensation delta on A's
  chain per the contract's declared policy.
- Both sides charge gas; A's gas budget bounds B's sub-execution.

Tests: snapshot pinning (B's chain advances between two A executors → identical A
result); revert policy (B-side failure → A settles deterministically); cross-shard
determinism for calling txs; finality independence (A finalizes without B's side
finalizing).

**Exit:** Model X semantics implemented and pinned by e2e + determinism tests.

### Phase 8 — End-to-end verification + docs

- **node-server composite e2e** (extend `node-server/src/node_server/tests/e2e.rs`):
  full pipeline `Sentinel Verify → Executor Preload → Finalizer Sign → Committer
  Commit` for (a) a standard transfer tx, (b) a `SpecEngine` contract tx, (c) a
  deployment tx, (d) a cross-contract call — asserting each committed block's
  `result_hash` equals an independently computed `hash(engine_output)`. This is the
  test that proves the executor "functions".
- **Cross-executor determinism test** (two `Executor` instances, same shard inputs →
  identical `result_hash` and identical `"Sign"` votes) — the sharding invariant,
  including a calling tx.
- Update `TASKS.md` (executor tail), README (Outstanding section), and the vault:
  `task-executor-contract-bytecode` → done/stale per its staleness signal; update
  `concept-executor-role` (pipeline steps changed), add event node; bump
  `roadmap-phase-status`.

**Exit:** e2e green; docs + vault current.

### Phase 9 — (Optional, Tier-2) Wasm engine

A `WasmEngine` implementing `ContractEngine` (wasmi, no-std-friendly, deterministic
feature set only) so third-party `SmartContract.bytecode` can be Wasm. Separate plan;
blocked on nothing in Phases 1–8.

---

## 4. Risks & mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Any engine nondeterminism (HashMap iteration, float, platform) | Same-parent conflicts, same-proposer slashing (ADR-008) | Phase-2 determinism property tests are a merge gate; engines use only BTreeMap/Vec canonical rmp; no floats in ISA |
| `payload` wire change breaks canonical-transaction signing | Rejected valid txs / replay issues | Ground Rule 4 additive pattern + `#[serde(default)]` + byte-identical regression test; `payload` included in `CanonicalTransaction` so it is covered by the sender signature |
| `result_hash` semantics change what blocks commit | All downstream block hashes change (intended) | This is the point of the work; e2e test pins the new semantics |
| D3 partition fix changes what the executor can see | Executions that "worked" on mis-keyed data (they didn't — D3 makes them fail) | D3 is a fix, not a behavior change, against any real data service |
| Engine bugs (overflow, gas accounting) | Failed txs, wasted gas | Fails closed to `Failed` state; no vote emitted on any engine error |
| Scope creep toward a full EVM | Slip | Tier-1 ISA is closed and small; Wasm is explicitly Phase 9; Q6 bounds the contract model (no cross-token effects, C6) |
| Cross-chain snapshot drift (B's state moves between A's execution and B's apply) | Nondeterministic A results → same-parent conflicts, slashing (ADR-008) | Model X pins `B@(height, hash)` inside the call; A's execution reads only the pinned ref; ADR-016 defines the validation |
| Non-atomic call failure leaves A and B inconsistent | User-visible broken state | Deterministic revert/compensation policy in ADR-016; contracts must be written to tolerate partial failure (documented constraint) |
| Governance (multisig/timelock) key management | Upgrades blocked by lost keys | Out of protocol scope (documented); M-of-N is the floor, stake-weighted voting is the documented extension |

## 5. Effort estimate (rough)

- P0: decisions only (one session).
- P1: 1–2 days (core module + wire field + literals).
- P2: 2–3 days (two engines + ISA doc + property tests).
- P3: 1–2 days (dispatch + D3/D4 fixes + executor tests).
- P4: 0.5–1 day.
- P5 (on-chain deployment): 2–3 days incl. ADR-015 design.
- P6 (upgrade governance): 2–3 days incl. ADR-017 design.
- P7 (cross-contract calls, Model X): 5+ days incl. ADR-016 design — highest design risk.
- P8: 1–2 days (e2e + docs + vault).
- P9: separate estimate.

Total: **~3–4 weeks** including the design ADRs (the base engine, P1–P4 + P8, is the
original ~6–9 days).

## 6. Standing vault duties (per CLAUDE.md)

Update on landing: `task-executor-contract-bytecode` (close), `concept-executor-role`
(pipeline steps), `decision-*` new nodes (P0: ADR-011–014; then ADR-015/017/016 at
the start of P5/P6/P7), `fact-*` nodes for D3/D4, `roadmap-phase-status`,
`_system/INDEX.md`, one log node per significant landing.
