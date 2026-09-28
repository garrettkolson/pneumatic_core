# P0 Implementation Plan — Lock Executor Contract-Execution Decisions

Status: **complete** (2026-09-28) — gate closed; W1–W6 executed. Q1–Q6 all
approved (2026-09-27/28, Garrett Olson); ADR-011–014 + 2 facts + task nodes
written; INDEX at 101 nodes; log `log-organize-vault-20260928-114211`.
Parent: `plans/executor-contract-execution-implementation-plan.md` (Phase 0)
Scope: **no code.** P0 is the decision gate for Phases 1–8: capture the six
design decisions (Q1–Q5 + Q6 contract model) as vault ADR nodes, record the two
verified wiring defects as fact nodes, and set up the task tracking. Everything
below is `infinite-brain/` + this plan file only; `git status` must show no changes
under `src/`, `executor/`, or any crate.

---

## 1. The gate: six decisions (four already approved)

W2 (writing the decision nodes) is **blocked on explicit user approval** — the vault
rules prohibit recording unverified/unapproved material as `decision` nodes.
Status after the decision walkthrough (closed 2026-09-28):

- **Approved (2026-09-27, Garrett Olson): Q1, Q2, Q4, Q6.**
- **Approved (2026-09-28, Garrett Olson): Q3** (gas/bounds — including the
  metered-cost model: declared `gas_limit` cap, ISA cost table, measured-cost
  settlement at commit, fixed base fee on failure) and **Q5** (ISA shape —
  versioned rmp AST with `Call`/snapshot-ref ops reserved for Phase 7).

W3–W6 are independent of the gate (the facts are code-verified today; the task
tracking is neutral to the decision outcomes). If a decision is amended at review,
the node bodies below carry the amended form. All four ADR nodes are written in one
batch after the gate fully closes.

| # | Decision | Recommendation | Rejected alternative | Status |
|---|----------|----------------|----------------------|--------|
| Q1 | Engine model | Pluggable native `ContractEngine` trait + name-keyed registry in `pneumatic_core` (spec-registry pattern); per-token selection via token metadata key `contract_engine`; Tier-1 = `TransferEngine` + `SpecEngine`; Wasm VM deferred to Tier-2 | Wasm from day one (dependency + determinism audit burden); fixed transfer-only (ignores the `SmartContract` model) | **Approved 2026-09-27** |
| Q2 | Calldata | Additive `payload: Vec<u8>` on `Transaction`, `#[serde(default, skip_serializing_if = "Vec::is_empty")]`, included in `CanonicalTransaction` (sender signs it); sentinel size cap; `tx.id` derived incl. payload; Ground Rule 4 wire pattern | Encode in `action`/`amount` (overloads a routing key); sidecar (unsigned calldata); defer | **Approved 2026-09-27** |
| Q3 | Gas / bounds | Per-tx instruction budget (engine-counted, default from `max_gas_limit`) + wall-clock `tokio::timeout` backstop (~5 s); payload bytes feed gas | Rely on `bid`/fuel accounting alone (no hard bound inside the executor) | **Approved 2026-09-28** |
| Q4 | State semantics | Executor is a **pure read-only function** of (canonical tx, token state, user state); output encodes intended state deltas; data service applies deltas at commit; fuel charging is a data-service concern | Executor writes state via `save_user`/`save_data` (write-ordering races between shard members) | **Approved 2026-09-27** |
| Q5 | Bytecode format | Versioned rmp-serializable instruction AST (small closed ISA) in `SmartContract.bytecode`, defined in `pneumatic_core` so all roles can parse/verify; `Call`/snapshot-ref ops reserved for Phase 7 | Wasm now (see Q1) | **Approved 2026-09-28** |
| Q6 | Contract model & lifecycle | Contract ≡ contract token (1:1); **on-chain deployment** (`DeployContract` tx, deterministic token id `H(deployer‖nonce‖bytecode_hash‖name)`, `CreateToken` commit delta); public execution; **cross-contract calls = Model X** (snapshot-pinned `B@(height,hash)`, non-atomic, deterministic revert/compensation); **upgrades = M-of-N owner multisig + 1-epoch timelock**; contracts move only their own token's units | Admin-gated genesis/upgrades; two-phase atomic cross-chain commit (kills optimistic finality); full stake-weighted voting; many-contracts-per-token | **Approved 2026-09-27** |

**Deliverable (W1):** this table, presented for approval (done for Q1/Q2/Q4/Q6).
Record the approver and date in `verified_by`/`verified_at` of each decision node at
write time.

## 2. Work items

### W1 — Decision review (gate)

Present §1 to the user; obtain approval (or amendments) for Q1–Q5. Nothing in W2
is written before approval.

### W2 — Write four ADR decision nodes (post-gate, one batch)

All in `infinite-brain/decisions/`, full frontmatter per
`_system/FRONTMATTER-SCHEMA.md` (validated against the checklist at the bottom of
that file), bodies 50–300 words with `file:line` evidence, matching the style of
`decision-executor-sharding.md` (ADR number in title, rationale paragraph,
rejected alternatives). ADR numbers **011–014** are allocated here (001–010 in use;
005/010 shared); **015–017 are reserved** for the follow-on design ADRs (deployment
mechanics, cross-chain call semantics, upgrade governance) written at the start of
Phases 5/7/6 respectively.

**W2a — `decisions/decision-contract-engine-model.md`**
- `id: decision-contract-engine-model`
- `title: "ADR-011: Pluggable native ContractEngine registry (Wasm deferred to Tier-2)"`
- summary: contract execution runs on a pluggable native `ContractEngine` trait
  selected per token by name (spec-registry pattern); Tier-1 ships `TransferEngine`
  + `SpecEngine` (documented rmp instruction AST); a Wasm VM is a Tier-2 engine,
  not the substrate.
- body captures: Q1 + Q5 (ISA format lives here as part of the engine-model
  decision), the determinism requirement (ADR-008 conflicts + same-proposer
  slashing make nondeterministic execution consensus-fatal), rationale
  (house spec-registry pattern: `TransactionValidationSpec`/`BlockValidatorSpec`,
  `block_validation_spec_name`), rejected alternatives.
- edges: `concept-executor-role` (related_to, 0.9); `decision-executor-sharding`
  (depends_on, 0.85 — shard members must agree); `decision-trait-based-abstraction`
  (derived_from, 0.8); `pillar-block-lattice` (related_to, 0.7);
  `task-executor-contract-bytecode` (supports, 0.8).

**W2b — `decisions/decision-tx-calldata-payload.md`**
- `id: decision-tx-calldata-payload`
- `title: "ADR-012: Additive payload: Vec<u8> on Transaction as contract calldata"`
- summary: contract input travels in a new additive `payload: Vec<u8>` field on
  `Transaction` (`#[serde(default)]`, part of `CanonicalTransaction` so the sender
  signs it); non-contract txs keep empty payload and byte-identical wire form.
- body captures: Q2, the Ground Rule 4 precedent (`SignedTransaction.shielded`,
  pneumatic-shielded-implementation-plan.md), the `CanonicalTransaction`/
  `verify_sender_signature` implication (`src/transactions.rs:183-191`), rejected
  alternative.
- edges: `concept-transaction-lifecycle` (related_to, 0.85); `fact-wire-protocol`
  (related_to, 0.8); `concept-transaction-state-machine` (related_to, 0.7);
  `task-executor-contract-execution` (supports, 0.8).

**W2c — `decisions/decision-executor-pure-read-only.md`**
- `id: decision-executor-pure-read-only`
- `title: "ADR-013: Executor is a pure read-only function; state deltas apply at commit"`
- summary: execution is a pure function of (canonical tx, token state, user state)
  — the executor reads state only via `DataProvider`, never writes; the canonical
  output encodes intended state deltas applied by the data service at commit;
  bounds are a per-tx instruction budget + wall-clock timeout.
- body captures: Q3 + Q4, why read-only (shard write-ordering races; horizontal
  scaling), state-model reference (`User`/`Account` in `src/user.rs`,
  `Token.asset_data`), gas-bounds policy.
- edges: `concept-executor-role` (related_to, 0.9);
  `decision-stake-snapshots-in-dataprovider` (related_to, 0.8 — data service as
  state owner); `concept-optimistic-finality` (related_to, 0.8);
  `concept-data-provider` (related_to, 0.75).

**W2d — `decisions/decision-contract-model-lifecycle.md`**
- `id: decision-contract-model-lifecycle`
- `title: "ADR-014: Contract model — contract-as-token, on-chain deployment, Model X calls, multisig+timelock upgrades"`
- summary: contract ≡ contract token (1:1); on-chain `DeployContract` with
  deterministic token id and a `CreateToken` commit delta; public execution;
  cross-contract calls are snapshot-pinned and non-atomic with deterministic
  revert (Model X); upgrades require M-of-N owner multisig + 1-epoch timelock;
  contracts move only their own token's units.
- body captures: C1–C6, the Model X rationale (preserves optimistic finality vs.
  two-phase atomic commit), rejected alternatives (admin-gated genesis/upgrades,
  many-contracts-per-token, full stake-weighted voting), and the reserved
  follow-on design ADRs (ADR-015 deployment, ADR-016 cross-chain calls,
  ADR-017 upgrade governance).
- edges: `concept-executor-role` (related_to, 0.9); `concept-per-token-chains`
  (related_to, 0.85 — calls span token chains); `concept-optimistic-finality`
  (related_to, 0.85 — Model X preserves it); `decision-executor-sharding`
  (related_to, 0.75); `pillar-block-lattice` (related_to, 0.7);
  `task-executor-contract-execution` (supports, 0.8).

### W3 — Record the two verified wiring defects as fact nodes

Both are verified against code today (grep evidence below), so they qualify as
`fact` nodes under the vault rule against recording unverified claims. Both in
`infinite-brain/facts/`.

**W3a — `facts/fact-executor-partition-key-defect.md`**
- `id: fact-executor-partition-key-defect`
- title: Executor passes env_id where the token partition id belongs
- summary: `Executor::run_execution` calls `get_data(key, &self.env_id)`
  (`executor/src/executor.rs:291,297`) while all other roles pass
  `env_data.token_partition_id` (e.g. `sentinel/src/sentinel/processing.rs:89`);
  against a real data service the fetch is keyed to a nonexistent partition and
  execution fails.
- evidence in body: the two call sites + the sentinel contrast.
- edges: `concept-executor-role` (related_to, 0.9); `concept-data-provider`
  (related_to, 0.7).

**W3b — `facts/fact-executor-backpressure-slot-leak.md`**
- `id: fact-executor-backpressure-slot-leak`
- title: Executor backpressure slots are never freed in production
- summary: `preload_cleanup` (`executor/src/executor.rs:137`) has no production
  callers (only tests); `run_execution` never removes its tx from
  `preload_tasks`, so after `max_in_flight` txs the executor is permanently
  `AtCapacity`.
- edges: `concept-executor-role` (related_to, 0.9).

### W4 — Task tracking

**W4a — edit `tasks/task-executor-contract-bytecode.md`** (the existing open task):
- body: status open → **planned**; add that decisions are locked (ADR-011–014),
  the plan is `plans/executor-contract-execution-implementation-plan.md`, and
  implementation is tracked by `task-executor-contract-execution`.
- keep the existing staleness signal (it stays true until the code changes).
- add edge: `task-executor-contract-execution` (followed_by, 0.8 — the stub task
  is superseded by the implementation task).
- bump `verified_at`/`verified_by`.

**W4b — `tasks/task-executor-contract-execution.md`** (new):
- `id: task-executor-contract-execution`
- title: Open: implement executor contract execution (plan Phases 1–8)
- summary: implement ADR-011–014 — core `ContractEngine` substrate + `payload` wire
  field, `Transfer`/`Spec` engines with determinism property tests, stub replacement
  + partition-key/slot-leak fixes, gas/timeout/panic bounds, on-chain deployment,
  upgrade governance, Model X cross-contract calls, composite e2e — per
  `plans/executor-contract-execution-implementation-plan.md` Phases 1–8.
- edges: `decision-contract-engine-model` (depends_on, 0.9);
  `decision-tx-calldata-payload` (depends_on, 0.8);
  `decision-executor-pure-read-only` (depends_on, 0.8);
  `decision-contract-model-lifecycle` (depends_on, 0.85); `concept-executor-role`
  (part_of, 0.9); `fact-executor-partition-key-defect` (related_to, 0.7);
  `fact-executor-backpressure-slot-leak` (related_to, 0.7).
- body: phase checklist (P1–P8 with exit criteria pointers; P5/P6/P7 each begin
  with their design ADR: 015/017/016) + status pending.

All edge targets above were verified to exist on disk
(`concepts/`, `facts/`, `decisions/`, `tasks/` listings, 2026-09-27).

### W5 — Sync `_system/INDEX.md`

- add rows for the 4 decisions, 2 facts, 1 task (id, title, summary, edge count);
- update the per-type header counts (decision 14→18, facts +2, tasks +1) and the
  Totals line (94 → 101);
- keep the "Last rebuilt" date current.

### W6 — Log node

One `logs/log-organize-vault-<YYYYMMDD-HHMMSS>.md` per the 8-field log schema
(`FRONTMATTER-SCHEMA.md` "Log Node Schema"): `operation: organize-vault`,
`affected_nodes` = the 7 new nodes + the 1 edited task node + `INDEX.md`,
30–80 word body (what ran, what changed, the 2026-09-27 approvals for Q1/Q2/Q4/Q6
and the pending Q3/Q5).

## 3. Sequencing

```
W1 (approval gate — Q1/Q2/Q4/Q6 approved; Q3/Q5 pending)
 ├─> W2 (4 decision nodes)          [blocked on Q3/Q5 closing]
 ├─> W3 (2 fact nodes)              [independent of W1]
 ├─> W4 (task edits + new task)     [independent of W1]
 └─> W5 (INDEX sync) + W6 (log)     [after W2–W4; W6 references all affected nodes]
```

W3 and W4 can run immediately (their content is neutral to Q3/Q5). W2 is written as
one batch once Q3/Q5 close, so all four ADR nodes land together with consistent
cross-references; W5/W6 are the final two writes of the session.

## 4. Verification & exit criteria

1. **No code touched:** `git status` shows changes only under `infinite-brain/`
   and `plans/`. No `cargo` invocation required.
2. **Frontmatter:** every new node passes the field-by-field checklist in
   `_system/FRONTMATTER-SCHEMA.md` (unique kebab id with type prefix, summary
   < 200 chars, non-empty conditional staleness signal, 2–8 kebab tags, edges all
   four keys).
3. **Graph integrity:** every edge target resolves to an existing node id; no new
   orphan (each new node has ≥ 1 edge); no contradictions introduced (the new
   ADRs must not conflict with ADR-008/009 or the optimistic-finality decision —
   read them before writing and cite consistency in the bodies).
4. **INDEX parity:** `INDEX.md` rows match files on disk exactly; totals correct.
5. **Log:** log node present, never-to-be-edited, lists all affected nodes.
6. **Gate recorded:** each ADR node's `verified_by`/`verified_at` reflect the
   actual approval (approver + date from W1).

## 5. Effort

One agent session. W1 is the only external dependency (user approval); the
vault writes (W2–W6) are ~15 minutes of mechanical work once the gate clears.

## 6. After P0

With the gate closed, Phase 1 (`src/contracts.rs` substrate + `payload` wire
field) is unblocked per the parent plan. The parent plan is now Phases 1–9:
base engine (P1–P4) → on-chain deployment (P5, ADR-015) → upgrade governance
(P6, ADR-017) → Model X cross-contract calls (P7, ADR-016) → composite e2e (P8)
→ Tier-2 Wasm (P9).
