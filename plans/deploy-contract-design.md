# ADR-015 Design: On-Chain Contract Deployment

**Status:** LANDED (Phase 6, 2026-09-29) — QD4 amended to `partition_id = environment_id`.
**Date:** 2026-09-29.
**Depends on:** ADR-011 (pluggable engines), ADR-013 (pure read-only executor /
canonical deltas), ADR-014 (contract ≡ token 1:1), ADR-018 (WasmEngine — P5 landed).

> **AMENDMENT (QD4, 2026-09-29 — operator override, landed in Phase 6):** the
> approved design used `partition_id == token_id` (each token is its own partition).
> **Garrett Olson overrode QD4: the deploy flow uses `partition_id = environment_id`.**
> The deterministic `token_id` (QD2), the `CreateTokenDelta`, and the committer
> re-derive + integrity check + idempotent apply are all unchanged; only the partition
> the token is stored under changes — the deploy token is saved in the **environment's
> token partition** (`save_token(token_id, token, environment_id)`), not a per-token
> partition. Every `partition_id = token_id` reference below is therefore read as
> `partition_id = environment_id`.

---

## 1. Problem

Today a contract token is created by an admin step or a test fixture, not by a
transaction. Phase 6 makes deployment a **protocol op**: a `DeployContract` tx
creates a contract token on-chain, **engine-agnostic** (both `Spec` rmp-AST and
`Wasm` module contracts). No admin step.

The executor stays **pure** (ADR-013): it does not write to the data service. It
emits a canonical `CreateToken` state delta in `result_data`; the data service
applies it at commit (idempotent). Consensus-critical pieces — the deterministic
`token_id`, the delta schema, the validation caps, the gas formula — are pinned in
this ADR so every shard member derives the same outcome.

## 2. Model: a contract is a token (ADR-014)

A contract **is** a `Token` with:
- `metadata["token_type"] = "contract"`
- `metadata["contract_engine"] = <engine name>` (e.g. `"Spec"`, `"Wasm"`)
- the bytecode in `asset_data` (a `SmartContract { name, bytecode, version }`)

Deployment = creating that token. **Each token is its own partition**
(`partition_id == token_id`; confirmed in `data.rs::latest_block_hash`, which does
`token_id = partition_id.as_bytes()`). This resolves the plan's "shard selection for
a not-yet-existing token id" item: there is no separate shard-selection step — the
new token's partition is simply its deterministic `token_id`.

## 3. The deployment flow

1. **Sender** issues a `DeployContract` tx:
   `{ action: "DeployContract", sender, payload, gas_limit, ... }`, where `payload`
   carries the deployment parameters (§6). `token_id` is **not** a target here —
   routing is by action, not by token (the target token does not exist yet).
2. **Sentinel** validates it with `DeployValidationSpec` (registered for the
   `"DeployContract"` action) — **fails closed before routing** (§7).
3. **Executor** special-cases `action == "DeployContract"` in its dispatch, **before**
   `select_engine` (there is no target token to select an engine for). It runs the
   pure `deploy_contract()` op, which computes the deterministic `token_id` and emits
   a `CreateTokenDelta` as `result_data` (§4–§5). The op is **not** a `ContractEngine`
   registry entry (QD1).
4. **Finalizer** signs the execution result (unchanged from the existing path).
5. **Committer** applies the delta at commit: reconstructs the `Token` and calls
   `save_token(token_id, token, partition_id = token_id)` — idempotent (§8).

## 4. Deterministic `token_id` (QD2)

CREATE2-style. The id is derivable **before** execution, identical on every shard
member, and replay-resistant:

```
token_id = SHA256( b"PNEUMATIC/DEPLOY/v1"
                   ‖ rmp_canon( deployer_pubkey,
                                deployer_nonce,
                                SHA256(bytecode),
                                name ) )
```

- `SHA256` is the protocol `HashProvider` (ring `BasicHashProvider`).
- The field order and the domain tag `b"PNEUMATIC/DEPLOY/v1"` are **pinned** here
  (consensus-critical; changing them is a hard fork).
- `deployer_nonce` = `User.nonce` (per-sender). A second deploy reusing the same
  nonce yields a *different* id at the id level, and `DeployValidationSpec` rejects
  the replay before routing (nonce check).
- `SHA256(bytecode)` (not the raw bytecode) keeps the rmp payload small for large
  (up to 1 MiB) Wasm modules.

## 5. The `CreateTokenDelta` (QD3)

A canonical rmp struct the executor emits in `result_data`:

```
struct CreateTokenDelta {
    token_id:  Vec<u8>,              // from §4
    name:      String,
    engine:    String,               // registered engine name
    bytecode:  Vec<u8>,              // Spec rmp-AST or Wasm module
    metadata:  HashMap<String,String>,  // extra user-supplied keys only
}
```

The committer reconstructs the `Token` from the delta: `id = token_id`,
`asset_data = bytecode`, and `metadata = { "token_type": "contract",
"contract_engine": engine, "name": name } ∪ delta.metadata`. The required
`token_type`/`contract_engine`/`name` keys are **set by the committer, not the
sender**, so a sender cannot forge a non-contract token or a wrong engine. No
`initial_state` in P6 — per-contract storage arrives with **W3 (Phase 7)**.
`SmartContract.version` defaults to `"1"`.

## 6. `DeployContract` tx payload

`payload` = rmp-canonical `DeployParams { name, engine, bytecode, metadata }`.
`sender`, `nonce`, and `gas_limit` come from the tx envelope and the sender's
`User` state (the nonce is loaded from the data service, as in standard nonce
validation).

## 7. `DeployValidationSpec` (QD5) — sentinel-side, fails closed

A `TransactionValidationSpec` registered for action `"DeployContract"`. It rejects
before routing if **any** of:
- **bytecode size** exceeds the engine cap — Wasm ≤ **1 MiB** (matches ADR-018/P5),
  Spec ≤ **64 KiB**;
- **`engine`** is not present in the `ContractEngineRegistry`;
- **`name`** is invalid — must be 1–64 bytes;
- **Wasm module check** — for `engine == "Wasm"`, the Phase-5 module validation must
  pass (well-formed, `__alloc`+`execute` exports, `env.*` import allow-list, no
  `f32`/`f64`);
- **nonce** != the sender's current nonce (replay protection).

## 8. Commit-time apply (idempotent)

`save_token(token_id, token, partition_id = token_id)`. **Idempotent:** if
`get_token(token_id)` already returns a token, re-apply is a no-op — a second block
re-emitting the same delta is harmless. Because the partition is the `token_id`
(§2), there is no separate shard-selection step.

## 9. Gas (QD6)

```
deploy_gas = DEPLOY_GAS_BASE + DEPLOY_GAS_PER_BYTE * bytecode.len()
```

Protocol-tunable constants — recommended **base = 50_000**, **per_byte = 10**. The
cost must be ≤ `gas_limit` or the deploy fails with `GasExhausted` (the 3-stage gas
model, ADR-013). This bounds the cost of a 1 MiB Wasm module at ~150_000 gas.

## 10. Code placement

- `pneumatic_core::contracts` (new `deploy` submodule): `CreateTokenDelta`,
  `DeployParams`, `derive_token_id(...)`, and the pure `deploy_contract(...) ->
  Result<CreateTokenDelta, ContractError>` op (the "DeployEngine").
- `pneumatic_core::validation`: `DeployValidationSpec` + registration.
- `executor`: dispatch special-case for `action == "DeployContract"` (runs the op,
  skips `select_engine`).
- `committer`: the idempotent `CreateTokenDelta` apply path (`save_token`).

## 11. Open decisions

| # | Decision | Recommendation |
|---|----------|----------------|
| **QD1** | Execution model | **Protocol op** (special-case in executor dispatch + pure core fn), *not* a `ContractEngine` registry entry — there is no deployed contract to select an engine for. |
| **QD2** | `token_id` formula | CREATE2-style, `SHA256` + domain tag + rmp-canon fields, as §4. |
| **QD3** | Delta schema + apply | `CreateTokenDelta` as §5; committer reconstructs the token and `save_token`s; idempotent; no `initial_state` in P6. |
| **QD4** | Partition / shard | **Resolved + AMENDED (operator override, landed Phase 6):** `partition_id = environment_id` (the deploy token is saved in the environment's token partition). Original design was `partition_id == token_id`. |
| **QD5** | Validation caps | Wasm ≤ 1 MiB, Spec ≤ 64 KiB; engine must be registered; name 1–64 B; Wasm module check; nonce check — all fail closed. |
| **QD6** | Gas constants | `base = 50_000`, `per_byte = 10` (protocol-tunable). |

## 12. Tests

- **Deterministic id:** same inputs → same id; different nonce → different id.
- **Replay:** a second `DeployContract` with the same nonce is rejected by
  `DeployValidationSpec`.
- **Deploy e2e (composite):** after commit, `get_token(token_id)` returns the new
  token, and a contract tx to it then executes.
- **Wasm module deploy:** a validated Wasm module is deployed, then its contract tx
  executes.
- **Idempotent re-apply:** applying the same `CreateTokenDelta` twice is a no-op.
- **Bytecode cap:** oversize bytecode is rejected.
- **Gas:** `deploy_gas = base + per_byte*len`; a `gas_limit` below it →
  `GasExhausted`.

## 13. Exit (per plan)

A contract can be created by a tx, not by an admin; engine-agnostic (Spec + Wasm);
e2e green (deploy → new token exists → its contract txs execute).
