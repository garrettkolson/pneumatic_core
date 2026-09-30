---
id: log-upgrade-governance-p8-20260930-120530
type: log
operation: organize-vault
date: "2026-09-30T12:05:30"
namespace: pneumatic
summary: "Phase 8 (upgrade governance, ADR-017) landed: owner registry + M-of-N multisig + 1-epoch timelock, executor re-validates quorum + emits ReplaceAssetDelta, committer apply under timelock; new decision-upgrade-governance node, task P8 marked complete, INDEX synced"
affected_nodes: ["decision-upgrade-governance", "task-executor-contract-execution", "decision-contract-deployment"]
tags: ["log", "organize-vault"]
---

Phase 8 of the executor contract-execution plan (upgrade governance, ADR-017) landed
2026-09-30. An `UpgradeContract` transaction swaps an existing contract's **bytecode +
owner set**, gated by an **M-of-N owner quorum** over a canonical Ed25519 digest and a
**1-epoch apply-time timelock**. Landed in `pneumatic_core::contracts::upgrade` (new
`src/contracts/upgrade.rs`), the `SmartContract` owner fields (`src/tokens.rs`:
`owners: Vec<Vec<u8>>` + `threshold: u32`, both `#[serde(default)]`; `threshold == 0` ⇒
immutable), `pneumatic_core::validation::upgrade` (`UpgradeValidationSpec`), the executor
dispatch (`execute_upgrade`: re-validates the quorum, emits the `ReplaceAssetDelta` in
`result_data`), and the committer `apply_upgrade_delta` (dispatch on `action ==
"UpgradeContract"`: re-derive delta, `hash(delta) == result_hash` else
`TransactionPayloadMismatch`, threshold-0 no-op (defense in depth), timelock gate
`epoch >= proposal_epoch + 1` else no-op, `apply_replace_asset`). **QD1** owner registry
= dedicated `SmartContract` fields; **QD2** quorum = canonical digest
`SHA256(b"PNEUMATIC/UPGRADE/v1" ‖ rmp(token_id, SHA256(bytecode), owners, threshold,
epoch))` + Ed25519 M-of-N over the **current** owners (safe rotation); **QD3** timelock =
apply-time gate, no pending store. Applies to Wasm modules (the delta swaps the module
bytecode). Created `decision-upgrade-governance` (ADR-017), marked
`task-executor-contract-execution` P8 ✅ (P1–P8 now landed; remaining P9 Model X, P10
e2e), and synced `_system/INDEX.md`. Workspace `cargo test` green: core 639, committer
109, executor 24, finalizer 61, node-server 32, prover 15, sentinel 57 (957 total, 0
failed).
