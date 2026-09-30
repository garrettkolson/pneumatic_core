---
id: decision-upgrade-governance
title: "ADR-017: Upgrade governance (owner registry + M-of-N multisig + 1-epoch timelock)"
type: decision
namespace: pneumatic
visibility: namespace
summary: "An UpgradeContract transaction swaps an existing contract's bytecode + owner set, gated by an M-of-N owner quorum over a canonical Ed25519 digest and a 1-epoch apply-time timelock; the executor re-validates the quorum and emits a re-derivable ReplaceAssetDelta the committer applies under the timelock. Applies to Wasm modules."
auto_inject: false
applicable_when: "Designing, scoping, or implementing contract upgrade governance, the owner registry, the multisig quorum, the timelock, or the committer's ReplaceAsset apply path"
confidence: 0.95
verified_at: "09/29/2026"
verified_by: "Garrett Olson"
staleness_signal: "Landed 09/29/2026 (Phase 8). Stale when the owner-registry model, the digest domain, the timelock length, or the multisig scheme changes, or when a proposal store / pending-upgrade queue is introduced"
tags: [adr, design-decision, contract-execution, upgrade, governance, multisig, timelock, owner-registry, committer]
edges:
  - target: decision-contract-model-lifecycle
    type: derived_from
    weight: 0.95
    note: "Implements ADR-014's M-of-N multisig + 1-epoch timelock upgrade path"
  - target: decision-contract-deployment
    type: related_to
    weight: 0.9
    note: "The upgrade path mirrors the deploy path: a re-derivable delta in result_data, integrity-checked + applied idempotently at commit"
  - target: decision-executor-pure-read-only
    type: depends_on
    weight: 0.9
    note: "The executor re-validates the quorum and emits a ReplaceAssetDelta in result_data (stays pure); the committer applies it at commit (ADR-013)"
  - target: decision-wasm-engine-tier2
    type: related_to
    weight: 0.85
    note: "Applies to Wasm modules: the ReplaceAssetDelta swaps the module bytecode, so a Wasm contract upgrades through the same governance path"
  - target: decision-contract-engine-model
    type: depends_on
    weight: 0.8
    note: "The new bytecode is engine-validated (Spec well-formedness / Wasm validate_wasm_module + scanner) before admission"
  - target: task-executor-contract-execution
    type: supports
    weight: 0.9
    note: "Phase 8 of the executor contract-execution plan"
  - target: decision-deterministic-leader-election
    type: related_to
    weight: 0.6
    note: "Shares the ADR-008 determinism requirement: the canonical upgrade digest + re-derived delta are identical on every shard member"
related: ["[[plans/upgrade-governance-design.md]]"]
source_url: "plans/upgrade-governance-design.md"
---

# ADR-017: Upgrade governance (owner registry + M-of-N multisig + 1-epoch timelock)

**Approved by Garrett Olson, 2026-09-29.** Full design: `plans/upgrade-governance-design.md`.
**Phase 8 landed 2026-09-29.** An `UpgradeContract` transaction swaps an existing
contract's **bytecode + owner set**, gated by an **M-of-N owner quorum** over a canonical
Ed25519 digest and a **1-epoch apply-time timelock**. Landed in
`pneumatic_core::contracts::upgrade` (new `src/contracts/upgrade.rs`), the
`SmartContract` owner fields (`src/tokens.rs`), `pneumatic_core::validation::upgrade`
(`UpgradeValidationSpec`), the executor dispatch (`execute_upgrade`), and the committer
apply path (`committer::committing::apply_upgrade_delta`). Workspace `cargo test` green
(core 628 → 639, committer 102 → 109, executor 21 → 24).

**Model (QD1–QD3 locked 2026-09-29):**
- **QD1 — owner registry:** a dedicated `SmartContract` field pair — `owners: Vec<Vec<u8>>`
  (owner public keys) + `threshold: u32` — both `#[serde(default)]` (legacy contracts are
  empty / immutable). `threshold == 0` ⇒ the contract is **immutable** (no upgrade
  admissible).
- **QD2 — quorum:** a **canonical upgrade digest** + **Ed25519 M-of-N** signatures.
  `digest = SHA256(UPGRADE_DOMAIN ‖ rmp_canon(token_id, SHA256(new_bytecode),
  new_owners, new_threshold, proposal_epoch))`, `UPGRADE_DOMAIN = b"PNEUMATIC/UPGRADE/v1"`.
  The quorum counts **distinct current owners** whose signature verifies the digest and
  authorizes iff `count >= threshold` (the **current** threshold, so an owner rotation is
  safe — the new owners take effect only after the swap). The **executor re-validates** the
  quorum deterministically (the sentinel admits it; the committer binds it).
- **QD3 — timelock:** an **apply-time gate**, no pending store. The committer swaps the
  asset only when the committed block's `epoch_number >= proposal_epoch + 1`
  (`TIMELOCK_EPOCHS = 1`). An early commit is a **no-op** — the proposal is admitted
  (sentinel) and signed (executor/finalizer) but not yet appliable; a later block at the
  right epoch applies it.

**Wire + re-derivation:** `UpgradeParams = { new_bytecode, new_owners, new_threshold,
proposal_epoch, owner_signatures }` (the tx payload). The executor emits a canonical
`ReplaceAssetDelta = { token_id, new_bytecode, new_owners, new_threshold, proposal_epoch }`
in `result_data` and the finalizer signs its hash. The delta is a **pure function** of the
transaction, so the committer **re-derives** it, verifies `hash(delta) == result_hash`
(`TransactionPayloadMismatch` otherwise), and — only under the timelock — applies it via
`apply_replace_asset` (swaps bytecode + owners + threshold; preserves name/version/storage).
**Gas = `50_000 + 10·len`** (flat base + per-byte). **Immutable** (`threshold == 0`) is
re-checked at the committer (defense in depth — sentinel + executor already reject). The
apply is a **no-op** for a missing token, an unsatisfied timelock, an immutable target, and
is **idempotent** under replay.

**Applicability to Wasm:** the `ReplaceAssetDelta` swaps the module bytecode, so a **Wasm
contract upgrades through the same governance path** — the new module is engine-validated
(`validate_wasm_module` + the deploy-time scanner) before admission, then swapped in at the
timelock. No Wasm-specific machinery; the delta is engine-agnostic.
