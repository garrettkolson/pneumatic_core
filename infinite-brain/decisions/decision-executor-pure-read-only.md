---
id: decision-executor-pure-read-only
title: "ADR-013: Executor is a pure read-only function; state deltas apply at commit"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Executor is a pure read-only function of (canonical tx, token state, user state); output encodes state deltas applied at commit; cost is a metered instruction budget vs a declared gas_limit plus timeout backstop."
auto_inject: false
applicable_when: "Designing execution output, state application, gas metering, or executor/data-service boundaries"
confidence: 0.95
verified_at: "09/27/2026"
verified_by: "Garrett Olson"
staleness_signal: "Stale when the executor writes state directly, deltas stop applying at commit, or gas metering is replaced by off-chain estimation"
tags: [adr, design-decision, executor, gas, state-delta, determinism]
edges:
  - target: concept-executor-role
    type: related_to
    weight: 0.9
    note: "Defines what the executor may and may not do with state"
  - target: decision-stake-snapshots-in-dataprovider
    type: related_to
    weight: 0.8
    note: "The data service is the state owner; deltas are its commit-time input"
  - target: concept-optimistic-finality
    type: related_to
    weight: 0.8
    note: "The first signature finalizes, so the delta must be identical on all shard members"
  - target: concept-data-provider
    type: related_to
    weight: 0.75
    note: "Executor reads state only through DataProvider fetches"
related: []
source_url: "plans/executor-contract-execution-implementation-plan.md (Q3, Q4)"
---

# ADR-013: Executor is a pure read-only function; state deltas apply at commit

Approved by Garrett Olson, 09/27/2026.

Execution is a pure function: `result = f(canonical_tx, token_asset_state,
user_state)`. The executor reads state only via `DataProvider`
(`User { public_key, fuel_balance, stake, nonce }`, `src/user.rs:6-15`; per-token
`Account` balances in `Token.asset_data`) and never writes. The canonical output
(`result_data`, rmp-canonical) encodes the **intended state delta**; the data
service applies deltas at commit time (the committer is already the persisting
role). This keeps sharding safe (no write-ordering races between shard members) and
keeps the executor horizontally scalable.

**Gas model — metered, not estimated** (arbitrary bytecode cannot be priced
statically; determinism is what makes metering network-consistent):

1. **Prepay cap**: the sender declares `gas_limit: u64` (additive, signed); the
   sentinel fails closed when `fuel_balance < gas_limit`.
2. **Metered execution**: the engine counts instructions against the cap using a
   protocol-specified cost table versioned with the ISA (ADR-011); over-budget →
   `GasExhausted` → `Failed`, no vote.
3. **Settlement at commit**: the data service charges the *measured* cost (not the
   cap) from `fuel_balance`; a failed tx charges a fixed base fee so failures are
   not free. Plain (non-contract) transfers keep the existing
   `base_cost + amount × multiplier` fee formula (`src/action_router.rs:825-841`).

**Alternative rejected**: executor writes state via `save_user`/`save_data` —
introduces write-ordering races between shard members and couples execution to the
data service's write path.
