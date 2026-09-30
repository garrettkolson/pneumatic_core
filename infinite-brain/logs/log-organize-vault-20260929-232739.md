---
id: log-organize-vault-20260929-232739
type: log
operation: organize-vault
date: "2026-09-29T23:27:39"
namespace: pneumatic
summary: "Phase 7 (Wasm storage W3, ADR-018) landed: sload/sstore/sdelete + storage delta in result_data, additive Transaction.result_data wire field, committer apply_storage_delta, storage gas + 1 MiB cap; ADR-018 + task P7 marked complete, INDEX synced"
affected_nodes: ["decision-wasm-engine-tier2", "task-executor-contract-execution"]
tags: ["log", "organize-vault"]
---

Phase 7 of the executor contract-execution plan (WasmEngine state & storage, W3) landed
2026-09-29. Per-contract `sload`/`sstore`/`sdelete` host imports (read-your-writes) write
a `StorageDelta = BTreeMap<Vec<u8>, Option<Vec<u8>>>` (`None` = tombstone) into the Wasm
`result_data` envelope (`WasmResult { module_output, storage_delta }`). Transport (QD1):
new additive `Transaction::result_data: Vec<u8>` wire field — the delta is the module's
`sstore` output and NOT re-derivable, so the executor sets it post-exec and the committer
consumes it. Location (QD2): `SmartContract::storage: BTreeMap<Vec<u8>, Vec<u8>>`.
Gas + cap (QD3): sload 100 / sstore-new 20000 / sstore-rewrite 5000 / sdelete 5000;
post-apply total ≤ 1 MiB (exceed → Reverted). New committer `apply_storage_delta`
(dispatch on `action == "ContractCall"`): verify `hash(result_data) == result_hash`,
decode the `WasmResult`, apply to storage — Wasm-only, idempotent. `WASM_OUTPUT_CAP`
reduced 64 KiB → 16 KiB (module heap constraint). Marked `decision-wasm-engine-tier2`
W3 landed and `task-executor-contract-execution` P7 ✅ (P1–P7 now landed). Synced
`_system/INDEX.md`. Workspace `cargo test` green: core 628, committer 102+9, executor
21, finalizer 61, node-server 32, prover 15, sentinel 57. Cross-executor Wasm
determinism re-verified (`cross_executor_determinism` ok).
