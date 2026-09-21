---
id: concept-action-router
title: "ActionRouter: per-action nonce/gas/stake gating and dispatch"
type: concept
namespace: pneumatic
visibility: namespace
summary: "ActionRouter routes Messages by action string: Process→nonce+gas, Preload→gas+executor stake, Sign→finalizer stake, Register→sentinel stake; results are typed ActionRouterResult variants."
auto_inject: false
applicable_when: "Adding a message action, changing nonce/gas/stake checks, or wiring node-to-node dispatch"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If route()'s action match arms, check_nonce/verify_gas/check_stake semantics, or ActionRouterResult variants change"
tags: [routing, nonce, gas, stake]
edges:
  - target: concept-pending-tx-registry
    type: depends_on
    weight: 0.8
    note: "Holds a shared Arc<RwLock<PendingTransactionRegistry>> for nonce coordination (action_router.rs:68)"
  - target: concept-env-driven-config
    type: related_to
    weight: 0.75
    note: "Gas formula and multiplier come from the environment CostModel; per-type stake floors from Config (action_router.rs:154-158, 183-184)"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.7
    note: "Dispatches on the Message action string carried in the wire envelope"
  - target: pattern-fail-closed
    type: supports
    weight: 0.65
    note: "amount: None is rejected with InvalidAmount before gas/stake checks (action_router.rs:216-218, 234-236)"
related: []
source_url: "Empty"
---

# ActionRouter: per-action nonce/gas/stake gating and dispatch

`IActionRouter` (`src/action_router.rs:21`) is the trait (`async fn route(Message) -> ActionRouterResult`); `ActionRouter` (line 64) is the implementation, holding an `EnvironmentMetadata`, a shared `PendingTransactionRegistry`, a `Config`, and a `DataProvider`.

`route` (lines 206-279) matches the wire action: **"Process"** deserializes the tx, checks nonce then gas, and returns `NonceUpdated` (lines 208-227); **"Preload"** verifies gas with the Preload multiplier then executor-type stake (lines 228-240); **"Sign"** checks finalizer-type stake (lines 241-246); **"Register"** checks sentinel-type stake (lines 259-263); "Confirm" forwards to committers; "Reject"/"Clear" reset the sender's nonce to 0; "DistributeToken" dispatches a token; anything else is `UnknownAction`. Results are typed variants (lines 29-56): `NonceUpdated`, `GasVerified`, `StakeChecked`, `TokenDispatched`, `Forwarded`, `UnknownAction`.

The three primitives: `check_nonce` (line 128) requires the data-store `User.nonce` to exactly equal `tx.sequence_number`, else `InvalidNonce`; `verify_gas` (line 150) computes `base_cost + amount × multiplier_for_action` and rejects with `GasLimitExceeded` if it exceeds `fuel_balance`; `check_stake` (line 175) enforces **two floors** — the environment's `global_min_stake` and the per-type `min_stake` from Config — returning `InsufficientStake` if either fails. A `None` amount is rejected up front as `InvalidAmount` before any gas or stake check (lines 216-218).
