---
id: log-organize-vault-20260726-testmodule-fix
type: log
operation: organize-vault
date: 2026-07-26T00:00:00
namespace: pneumatic
summary: "Fixed the finalizer/executor test-module compile break the user hit (3 errors) and recorded a workflow gotcha: plain `cargo test` at this root runs ONLY the root pneumatic_core package, so the member-crate test modules (finalizer, executor, ...) were never compiled — use `cargo test --workspace`. Fixes: block_builder make_test_keypair missing .expect() on VerifyingKey::from_bytes; removed dead make_test_signing_key (referenced removed SigningKey); rewrote stale executor signing.rs to the current ExecutorHandle::send_to_finalizer 4-arg Preload+Sign API. Full workspace suite now green: 835 passed / 37 ignored / 0 failed."
affected_nodes: ["fact-test-suite-s6", "fact-test-suite", "_system/INDEX", "repo:finalizer/src/block_builder.rs", "repo:finalizer/src/finalizer/tests/helpers.rs", "repo:executor/src/executor/tests/signing.rs"]
tags: ["log", "organize-vault", "test-module", "compile-fix", "workspace-flag", "finalizer", "executor"]
---

# Log: organize-vault — finalizer/executor test-module compile fix

The user reported the finalizer "is not currently compiling" (3 errors). All were in **test modules** that plain `cargo test` never compiles at this workspace root — hence they went undetected by the earlier (root-package-only) test runs.

## The gotcha (the important finding)

This is a workspace **with a root package** (`pneumatic_core`). Plain `cargo test` at the root runs **only the root package** (~559 tests). It does **not** build or run the member crates (committer, executor, finalizer, sentinel, node-server, prover). **`cargo test --workspace` is required** to compile + run every crate's `#[cfg(test)]` modules. Recorded on `fact-test-suite-s6`.

## The three compile errors fixed

1. `finalizer/src/block_builder.rs:374` — `make_test_keypair()` returned a bare `Result` from `VerifyingKey::from_bytes(&pk_bytes)`. Added `.expect("valid verifying key")`. (Left over from the hybrid-signing rewrite.)
2. `finalizer/src/finalizer/tests/helpers.rs:255` — dead `make_test_signing_key()` referenced the removed `SigningKey` type (E0412/E0433). Deleted the unused helper.
3. `executor/src/executor/tests/signing.rs` — stale Phase-1.1 test calling the old 3-arg `Executor::send_to_finalizer` that emitted `"Execute"`. The prior session moved `send_to_finalizer` to `ExecutorHandle`, made it 4-arg `(tx_id, tx_bytes, execution_result, result_hash)`, and changed it to emit `"Preload"`+`"Sign"`. Rewrote the test to the current API: `executor.clone_handle().send_to_finalizer(...)`, assert 2 captured messages both signed by the executor identity, neither under the destination key.

## Result

Full `cargo test --workspace` green: **835 passed / 37 ignored / 0 failed** (was 834 — the +1 is the e2e `pipeline_integration` test now counted under the full-workspace run).

## Nodes updated

- `fact-test-suite-s6` → 835/37/0, `verified_at` 07/26/2026, +`workspace-flag` tag, + the `--workspace` gotcha in the body.
- `fact-test-suite` → left as the 09/23 historical 828/32/0 baseline (briefly mis-edited, then reverted).
- `_system/INDEX` → the s6 baseline row summary updated.
