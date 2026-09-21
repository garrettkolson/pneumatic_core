---
id: pattern-per-sender-rmw-mutex
title: "Per-sender read-modify-write mutex for balance deductions"
type: pattern
namespace: pneumatic
visibility: namespace
summary: "DashMap of sender key to Arc<Mutex<()>> serializes read-modify-write (get, subtract, save) per sender while distinct senders stay fully concurrent; failures surfaced, never swallowed."
auto_inject: false
applicable_when: "Adding a new concurrent read-modify-write over shared per-entity state (balances, counters) in a role crate"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when committer/src/committer.rs rmw_mutex / gas deduction logic (AUDIT 4.5/M11 site) changes"
tags: [concurrency, mutex, gas, balances, audit]
edges:
  - target: concept-committer-role
    type: related_to
    weight: 0.9
    note: "The pattern's origin: commit-time gas deduction from the sender's fuel balance (committer.rs:343-355, 537-562)"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.8
    note: "Complementary: the lock prevents lost updates, gas_deduction_err (committer.rs:360-382) surfaces RMW failures with a greppable log line"
related: []
source_url: "Empty"
---

# Per-sender read-modify-write mutex for balance deductions

A targeted concurrency pattern from the committer's commit path (AUDIT Phase 4.5 / M11). Problem: gas deduction is a read-modify-write over the sender's stored `fuel_balance` (get_user → subtract → save_user), and two concurrent commits for the *same* sender could each read the old balance and lose one deduction.

The fix, as implemented (`committer/src/committer.rs:343-355`): a per-sender guard map — `rmw_mutex(sender)` returns an owned `Arc<Mutex<()>>` from a `DashMap<Vec<u8>, Arc<Mutex<()>>>`, creating it on first use. The lock is held only across the get/subtract/save sequence, so:

- distinct senders proceed in full concurrency (no global lock, no cross-sender blocking);
- same-sender commits serialize (no lost update).

The owned-`Arc` return is deliberate — it keeps the returned lock from being tied to the brief lifetime of the DashMap lookup. The pattern pairs with fail-closed error surfacing: any `get_user`/`save_user` failure builds `CommitterError::GasDeduction` and emits a prominent `GAS DEDUCTION FAILED:` log line so a silently-free-gas condition is observable (`committer.rs:360-382`), and the deduction is guarded by `if let Some(gas_used) = ...` so gas-free transactions are not treated as errors (`committer.rs:547`).

Generalizes to any per-entity counter (stake, nonces, quota) where entity-scoped serialization is cheaper than entity-scoped sharding.
