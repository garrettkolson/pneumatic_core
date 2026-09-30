---
id: decision-contract-deployment
title: "ADR-015: On-chain contract deployment (engine-agnostic Spec+Wasm)"
type: decision
namespace: pneumatic
visibility: namespace
summary: "A DeployContract transaction deploys a contract as a 1:1 contract token: a deterministic CREATE2 token id, a CreateToken delta emitted in result_data (executor stays pure), re-derived + integrity-checked at commit and applied idempotently; engine-agnostic (Spec+Wasm), partition_id = environment_id."
auto_inject: false
applicable_when: "Designing, scoping, or implementing on-chain contract deployment, the deploy validation spec, or the committer's deploy-token apply path"
confidence: 0.95
verified_at: "09/29/2026"
verified_by: "dsh-agent"
staleness_signal: "Core landed (Phase 6, 09/29/2026); W3 storage (Phase 7) + upgrade governance (Phase 8 / ADR-017) landed 09/29/2026 and do not change the CreateTokenDelta. Stale when the partition model (QD4) changes or a new deploy parameter is added"
tags: [adr, design-decision, contract-execution, deployment, token-id, create-token-delta, committer, engine-agnostic]
edges:
  - target: decision-contract-model-lifecycle
    type: derived_from
    weight: 0.95
    note: "Implements ADR-014's on-chain deployment + deterministic token id; contract ≡ token 1:1"
  - target: decision-executor-pure-read-only
    type: depends_on
    weight: 0.9
    note: "Executor emits a CreateToken delta in result_data and stays pure; the committer re-derives + applies at commit (ADR-013)"
  - target: decision-contract-engine-model
    type: depends_on
    weight: 0.85
    note: "Engine-agnostic: the deploy validation spec verifies the target engine is a registered ContractEngine (Spec+Wasm)"
  - target: decision-wasm-engine-tier2
    type: related_to
    weight: 0.7
    note: "A Wasm module deploys through the same engine-agnostic path; module-size cap + validate_wasm_module gate apply"
  - target: task-executor-contract-execution
    type: supports
    weight: 0.9
    note: "Phase 6 of the executor contract-execution plan"
  - target: concept-contract-scanner
    type: related_to
    weight: 0.8
    note: "The deploy-time contract scanner (S0) is check 4b in this spec's validation pipeline — a Reject finding fails the deploy with ContractScanFailed"
  - target: decision-deterministic-leader-election
    type: related_to
    weight: 0.6
    note: "Shares the ADR-008 determinism requirement: same (deployer, nonce, bytecode, name) => same token id + result_hash on every shard member"
related: ["[[plans/deploy-contract-design.md]]"]
source_url: "plans/deploy-contract-design.md"
---

# ADR-015: On-chain contract deployment (engine-agnostic Spec+Wasm)

**Approved by Garrett Olson, 2026-09-28.** Full design: `plans/deploy-contract-design.md`.
**Phase 6 landed 2026-09-29.** A `DeployContract` transaction deploys a contract as a 1:1
contract token (ADR-014). Landed in `pneumatic_core::contracts::deploy` (new
`src/contracts/deploy.rs`), `pneumatic_core::validation::deploy`
(`DeployValidationSpec`), the executor dispatch (`run_execution` special-case for
`action == "DeployContract"`), and the committer apply path
(`committer::committing::apply_deploy_delta`). Workspace `cargo test` green (core 601).

**Model (QD1–QD6 locked 2026-09-28):**
- **QD1** — a `DeployContract` **protocol action** (not a registry engine); the executor
  special-cases the action and the sentinel validates it with a dedicated
  `DeployValidationSpec`.
- **QD2** — **deterministic CREATE2 token id**:
  `SHA256(b"PNEUMATIC/DEPLOY/v1" ‖ rmp{deployer, nonce, SHA256(bytecode), name})`.
  Pure function of `(sender, nonce, DeployParams)` → identical on every shard member.
- **QD3** — a **`CreateTokenDelta`** (token_id, name, engine, bytecode, metadata) is
  emitted in `result_data`; the executor stays pure (ADR-013). **No initial_state**
  (storage is Phase 7 / W3).
- **QD4** — **`partition_id = environment_id`** (operator override of the original
  `token_id` proposal). The deploy token is stored in the environment's token partition.
- **QD5** — fail-closed caps: Wasm module ≤ 1 MiB (+ `validate_wasm_module`), Spec
  bytecode ≤ 64 KiB, target engine must be registered, name 1–64 bytes, deployer nonce
  must equal the account sequence number, `risk.score() ≤ max_risk`.
- **QD6** — **gas = `50_000 + 10·bytecode_len`** (flat deploy cost + per-byte).

**Committer re-derivation + integrity check:** the committer never trusts the raw
`result_data` bytes. It re-derives the `CreateTokenDelta` as a pure function of
`(sender, nonce, DeployParams)` using a local `BasicHashProvider`, verifies
`hash(delta) == tx.result_hash` (else `TransactionPayloadMismatch`), then applies the
token **idempotently** (`get_token` → no-op if present, else `save_token`) so a replay
cannot double-apply. The committer-forged metadata keys (engine, token_type) win over
any sender-forged values.

**Contract scanner (S0, landed 2026-09-29)** — a deterministic, fail-closed deploy-time
pre-screen (`concept-contract-scanner`, `src/contracts/scan.rs`) added as check 4b in
`DeployValidationSpec` (after the Wasm module check, before the nonce check). It runs the Spec
well-formedness check, a `wasmparser` Wasm static walk (catches internal `f32`/`f64` opcodes the
ABI-boundary check misses, disallowed imports), and a canary `execute` on a fixed canonical input
+ `CANARY_FUEL_BUDGET` (catches gas-burn loops / output spam). A `Reject`-severity finding fails
the deploy with the new `ValidationFailureReason::ContractScanFailed`; `Warn` findings are logged
(v1). Pure function of `(engine, bytecode)` + frozen caps → identical verdict on every shard
member (ADR-008). `wasmparser = "=0.239.0"` added as a direct dep (`default-features = false` to
avoid the `indexmap/serde` bump). Core suite 601 → 623.
