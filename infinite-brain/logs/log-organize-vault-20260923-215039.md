---
id: log-organize-vault-20260923-215039
type: log
operation: organize-vault
date: 2026-09-23T21:50:39
namespace: pneumatic
summary: "Landed monolith-modularization steps 6+7, closing the program: core auth::authenticate_envelope C1 primitive (+5 tests) with finalizer×2 + committer×1 call-site conversions, and the executor tests-only move (executor.rs 1081→528). Workspace 828/32/0, warnings 185→185."
affected_nodes: ["task-monolith-modularization", "pattern-authenticated-voter-c1", "fact-test-suite", "_system/INDEX"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — C1 auth helper + executor tests move (steps 6–7, program closed)

Refactor steps 6 and 7 (task-monolith-modularization) landed in one pass; the
task is now CLOSED — all seven steps done.

**Step 6 — core C1 helper.** New `pneumatic_core::auth` (`src/auth.rs`):
`authenticate_envelope(crypto, registry, signature, public_key, body) ->
Result<Vec<NodeRegistryType>, EnvelopeAuthError>` — the two shared C1 steps
(envelope verify incl. the `check_signature` `Ok(false)`-means-mismatch trap,
then registry role resolution; fail closed), returning the sender's full
resolved role set. `EnvelopeAuthError::Signature { reason: Option<String> }`
keeps provider-error vs clean-mismatch distinguishable for callers' message
formatting. 5 core tests: registered signer, composite multi-role resolution,
tampered body, signature claimed under a different registered key, valid
envelope from an unregistered key.

**Correction to the analysis:** the "5 production sites" count was 2-for-5
wrong — `block_services.rs:221` and `transaction_notifier.rs:323` are
`#[cfg(test)] assert_signed_by` test helpers. The true production surface was
3 functions, all converted to delegate:

- `finalizer/src/finalizer/signing.rs::authenticate_signature_message` (+ local Executor gate)
- `finalizer/src/finalizer/shielded.rs::authenticate_shielded_message` (+ local Finalizer gate)
- `committer/src/committer.rs::authenticate_message` (+ local action→roles gate)

All exact rejection strings preserved (the finalizer tests assert
"not registered as any node" / "not a Finalizer" substrings); committer keeps
swallowing provider errors into `UnauthenticatedSender`. The sentinel C3 shape
(envelope sender == `tx.sender`, no registry gate) stays out of the helper by
design.

**Step 7 — executor tests move.** `executor/src/executor.rs` 1081→528 ln;
production untouched (the crate is cohesive, as the analysis predicted). Test
region → `executor/src/executor/tests/{helpers, lifecycle, validation,
signing}`: 10 tests + 8 helpers (6 env/config/registry builders,
RecordingConnection — field pub-ified — and assert_signed_by), first-compile
green.

**Gates:** `cargo test --workspace` = **828 passed / 0 failed / 32 ignored**
(823 + the 5 new core auth tests; everything else count-neutral); workspace
warning total **185 → 185** (verified by stash/HEAD diff — zero new warnings;
the one new `unused_mut` in auth.rs tests was fixed in place).

Vault: pattern-authenticated-voter-c1 rewritten around the shared primitive
(including the test-helper correction + new staleness_signal), task steps 6–7
struck DONE with the deviations, task summary marked COMPLETED, fact-test-suite
re-baselined to 828/32/0, INDEX rows synced. Changes uncommitted for human
review (HEAD f2447bb).
