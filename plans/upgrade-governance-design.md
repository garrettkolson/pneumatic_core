# ADR-017 Design: Upgrade Governance (M-of-N Multisig + 1-Epoch Timelock)

**Status:** LANDED (Phase 8, 2026-09-29).
**Date:** 2026-09-29.
**Depends on:** ADR-011 (pluggable engines), ADR-013 (pure read-only executor / canonical
deltas), ADR-014 (contract ≡ token 1:1), ADR-015 (on-chain deployment — the owner registry
is settable at deploy), ADR-018 (WasmEngine — an upgrade swaps a Wasm module under the same
frozen ABI).

> **Design decisions locked 2026-09-29 (operator-approved, all "recommended"):**
> - **QD1 — Owner registry location:** dedicated `SmartContract` fields
>   (`owners: Vec<Vec<u8>>`, `threshold: u32`), part of the frozen-ABI contract asset,
>   settable at deploy. (Not a token-metadata string.)
> - **QD2 — Quorum scheme:** owners Ed25519-sign a canonical upgrade digest; quorum = at
>   least `threshold` distinct *current* owners verify. Executor re-validates.
> - **QD3 — Timelock:** apply-time gate, no pending store. The committer swaps the asset
>   only when the committed block's `epoch_number >= proposal_epoch + 1`; an early commit
>   is a no-op.

---

## 1. Problem

A deployed contract (a `Token` with a `SmartContract` asset, ADR-014) has no governed way
to change its bytecode after deployment. Phase 8 makes upgrades a **protocol op**: an
`UpgradeContract` tx swaps a contract's `asset_data` (the module) under **M-of-N owner
signatures** plus a mandatory **one-epoch delay**. It is engine-agnostic — it swaps a
`Spec` rmp-AST or a `Wasm` module alike — and the frozen ABI (ADR-018) guarantees old and
new modules speak the same interface, so an upgrade cannot break the host boundary.

The executor stays **pure** (ADR-013): it re-validates the quorum deterministically and
emits a canonical `ReplaceAsset` delta in `result_data`; the data service (committer)
applies it at commit — and only once the timelock has elapsed. Consensus-critical pieces —
the owner registry schema, the canonical upgrade digest, the quorum rule, the timelock rule,
the gas formula — are pinned in this ADR so every shard member derives the same outcome.

## 2. Model: an upgrade is a `ReplaceAsset` delta

An upgrade replaces the target token's `SmartContract` asset (the bytecode) and, optionally,
its owner set. It is a canonical, re-derivable delta:

```
ReplaceAssetDelta { token_id, new_bytecode, new_owners, new_threshold, proposal_epoch }
```

Because the new bytecode and owner set ride in the tx `payload` (`UpgradeParams`), the
committer can **re-derive** the delta from the transaction (exactly as it re-derives the
deploy `CreateTokenDelta`) and verify it against the signed `result_hash`. The upgrade is
therefore a pure function of `(tx, current token)`.

## 3. The upgrade flow

1. **Sender** issues an `UpgradeContract` tx with `token_id` = the target contract and a
   `payload` = `UpgradeParams` (§4–§6). Routing is by `token_id` (the target exists,
   unlike a deploy).
2. **Sentinel** validates it with `UpgradeValidationSpec` (registered for
   `"UpgradeContract"`) — **fails closed before routing** (§7). It checks the quorum against
   the *current* token's owners, the bytecode cap, and (for Wasm) the module check.
3. **Executor** fetches the target token (step 3 of the normal path), then runs
   `execute_upgrade`: it **re-validates the quorum deterministically** (same digest, same
   owners, same signatures — so the vote is inside the canonical `result_hash`), and emits
   the canonical `ReplaceAssetDelta` as `result_data`.
4. **Finalizer** signs the execution result (unchanged).
5. **Committer** applies the delta at commit (`apply_upgrade_delta`): re-derives it,
   verifies the hash, enforces the **timelock** against the committed block's
   `epoch_number`, and swaps the token's asset (§6) — idempotent.

## 4. Owner registry (QD1)

The registry is part of the `SmartContract` asset (frozen ABI):

```
SmartContract { name, bytecode, version, storage,
                owners: Vec<Vec<u8>>,   // Ed25519 verifying keys
                threshold: u32 }         // M-of-N; 0 == no one can upgrade
```

Both fields are `#[serde(default)]` (an empty owner set + zero threshold), so existing
contract tokens (including those deployed before this ADR) deserialize unchanged. The
registry is **settable at deployment**: `DeployParams` / `CreateTokenDelta` carry optional
`owners` + `threshold` (default empty / 0). Changing the owner set afterwards is itself an
`UpgradeContract` op gated by the *current* `threshold` (rotation).

A `threshold` of `0` means the contract is **permanently immutable** (no owner can upgrade
it) — the fail-closed default.

## 5. M-of-N quorum (QD2)

**Canonical upgrade digest** (what the owners sign — a pure function of the upgrade,
deterministic on every shard member):

```
upgrade_digest = SHA256( UPGRADE_DOMAIN
                 ‖ rmp_canon( token_id,
                              SHA256(new_bytecode),
                              new_owners,
                              new_threshold,
                              proposal_epoch ) )
```

`UPGRADE_DOMAIN = b"PNEUMATIC/UPGRADE/v1"`. `new_owners` is a `Vec<Vec<u8>>` serialized in
rmp-canonical order (the delta's owner set). Each owner signs `upgrade_digest` with their
Ed25519 key; the signatures ride in `UpgradeParams.owner_signatures: Vec<Vec<u8>>`.

**Quorum rule:** count the number of **distinct current owners** (from the target token's
*current* `SmartContract.owners`) whose signature verifies against `upgrade_digest`. The
upgrade is authorized iff that count `>= threshold` (the *current* threshold). Using the
*current* owners (not the proposed `new_owners`) is what makes owner-set rotation safe: a
malicious owner set can only take over if the *current* quorum authorized it.

Verification is a pure function of `(digest, signatures, current owners)` — the sentinel
runs it at admission, and the executor re-runs the identical check so the quorum outcome is
inside the canonical `result_hash` (a quorum-invalid upgrade fails the tx before commit).

## 6. Timelock (QD3)

`UpgradeParams.proposal_epoch: u64` is the epoch at which the upgrade was proposed. The
**committer** enforces the delay at apply time using the committed block's
`epoch_number` (the "epoch data from the committer"):

```
apply the asset swap  ⟺  block.epoch_number >= proposal_epoch + 1
```

If the committed block is in the proposal epoch (or earlier), the swap is a **no-op** — the
upgrade is not applied that epoch (there is no pending store; a later re-submission at the
next epoch applies it). This is deterministic, replay-safe, and needs no new persistent
state. The "early apply rejected" test asserts a commit at `proposal_epoch` leaves the
asset unchanged, while a commit at `proposal_epoch + 1` swaps it.

## 7. Validation (sentinel `UpgradeValidationSpec`)

`UpgradeValidationSpec` (registered for `"UpgradeContract"`) fails closed on any check:
1. `tx.payload` deserializes to `UpgradeParams`;
2. the target token exists and is a contract (`token_type == "contract"`);
3. the target's `threshold` is `> 0` (an immutable contract cannot be upgraded);
4. the quorum is met (≥ `threshold` distinct current owners sign the digest);
5. the new `bytecode` is within the engine cap (`Wasm` ≤ 1 MiB, else ≤ 64 KiB) and, for
   `Wasm`, passes `validate_wasm_module` + the deploy-time scanner (no `Reject` finding);
6. the composite risk does not exceed the environment's `max_risk`.

The timelock is **not** checked here (it is "at apply time" — the sentinel admits the
proposal; the committer gates the apply).

## 8. Gas

`upgrade_gas = UPGRADE_GAS_BASE + UPGRADE_GAS_PER_BYTE * len(new_bytecode)`
(`UPGRADE_GAS_BASE = 50_000`, `UPGRADE_GAS_PER_BYTE = 10` — same shape as `deploy_gas`).
A `gas_limit` below it → `GasExhausted` (the executor charges it in `execute_upgrade`).

## 9. Interaction with deployment

`DeployParams` / `CreateTokenDelta` gain optional `owners: Vec<Vec<u8>>` + `threshold: u32`
(`#[serde(default)]`), so a deploy can establish the owner registry in one op. A deploy
with no owners produces an immutable contract (`threshold = 0`).

## 10. Tests

- **Quorum edges:** `M-1` signatures rejected, `M` accepted (core + validation spec).
- **Digest determinism:** same inputs → same digest; a changed `new_bytecode` /
  `new_owners` / `proposal_epoch` → different digest (a signature for one upgrade does not
  authorize another).
- **Timelock:** a commit at `proposal_epoch` does NOT swap the asset; a commit at
  `proposal_epoch + 1` does.
- **Owner-set rotation:** a quorum-authorized owner change takes effect; the *current*
  threshold gates the rotation.
- **Engine-behavior swap:** a successful upgrade changes the engine behavior of subsequent
  txs, including swapping a `Wasm` module (the new bytecode's output differs).
- **Immutable contract:** a `threshold == 0` contract rejects an upgrade (spec + committer
  no-op).

## 11. Exit (per plan)

Upgrades are a governed on-chain act (M-of-N + 1-epoch timelock); the owner registry is
settable at deploy; a Wasm module can be swapped under governance; e2e green.
