---
id: concept-sentinel-role
title: "Sentinel role — gatekeeper: auth, validation, and routing"
type: concept
namespace: pneumatic
visibility: namespace
summary: "The gatekeeper: fail-closed sender auth, gas + spec validation; self-verified tokens route direct to Committer, standard txs to Executor; deterministic finalizer pick; shielded bypasses Executor."
auto_inject: false
applicable_when: "Working on sentinel/, transaction admission, routing, finalizer assignment, or shielded transfer intake"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when sentinel/src/sentinel.rs changes its action routing, C3 sender-auth steps, or the self-verified routing split"
tags: [sentinel, worker-crate, gatekeeper, routing, admission]
edges:
  - target: pattern-fail-closed
    type: related_to
    weight: 0.9
    note: "C3 sender authentication (empty sender, sender signature, envelope==sender) drops forged txs before gas/validation"
  - target: concept-executor-sharding
    type: related_to
    weight: 0.8
    note: "The sentinel's ExecutorSetCache + deterministic selection is the shard-aware routing half of sharding"
  - target: concept-shielded-pool-view
    type: related_to
    weight: 0.9
    note: "Advisory shielded validation reads nullifiers + root history from the shared pool view"
  - target: concept-optimistic-finality
    type: related_to
    weight: 0.7
    note: "Admission + enqueue is what eventually feeds the finalizer's optimistic commit"
  - target: concept-transaction-lifecycle
    type: related_to
    weight: 0.8
    note: "Owns the Preloaded→Validated transitions and the shielded-transfer intake path"
related: []
source_url: "Empty"
---

# Sentinel role — gatekeeper: auth, validation, and routing

The Sentinel is the first node in the pipeline and the gatekeeper role (`sentinel/src/sentinel.rs:22-32`): receive raw transactions, validate against the appropriate spec, route them, manage the `PendingTransactionRegistry`, and handle risk-based routing. Its `on_data_received` dispatches `Process`, `Confirm`, `Reject`, `Register`, `Clear`/`Delete`, `BlockFinalized`, and `ShieldedTransfer` (`sentinel.rs:122-142`).

`handle_process_request` (code-verified):

1. **Fail-closed sender auth (AUDIT C3)** — before gas or validation: `tx.sender` non-empty; the sender's Ed25519 signature over the canonical tx bytes verifies; and the gossiper-authenticated envelope sender equals `tx.sender`, so a peer cannot debit an account it doesn't own.
2. Compute gas used, run spec validation (self-verified tokens use the `SelfSigned` spec — `transaction_validator.rs:22-33`), record gas, and register the tx as pending.
3. **Routing split**: load the token; if `token.is_self_verified`, route directly toward commitment (`handle_self_signed`) — Executor + Finalizer skipped. Contract tokens default `block_validation_spec_name` to `SelfSigned` but keep `is_self_verified = false`, so the flag (not the spec name) is the discriminator (AUDIT 5.9).
4. Standard path: transition to `Validated` with risk (rejecting replayed nonces, Phase 5.6/H14) and send to Executors for preloading.

**Deterministic finalizer assignment**: `assign_finalizer_deterministic` loads the epoch stake snapshot, then delegates to `pneumatic_core::deterministic_select` over `FINALIZER_DOMAIN` with the latest block hash as salt; a zero-stake winner is a routing error (`sentinel.rs:685-720`); a rejected finalizer triggers a `_retry` suffix reselection (`sentinel.rs:721-736`).

**Shielded transfers** (`sentinel.rs:144-221`): five fail-closed steps — canonical deserialize, advisory `validate_shielded` against the shared `ShieldedValidationDeps` (pool-view nullifiers + root history + recency window), token gates (resolvable, self-verified, shielded opt-in), atomic registration in the never-evicted shielded map, then deterministic finalizer assignment + `SignShielded` fan-out. Shielded txs **bypass the Executor entirely** — value moves in-circuit, nothing to execute.
