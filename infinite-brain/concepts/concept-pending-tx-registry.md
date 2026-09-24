---
id: concept-pending-tx-registry
title: "PendingTransactionRegistry: in-flight tx state with replay-proof nonce tracking"
type: concept
namespace: pneumatic
visibility: namespace
summary: "PendingTransactionRegistry (registry.rs:31) tracks in-flight transactions in a DashMap plus an ordered per-token pool, gas usage, admin credits, and an append-only used_nonces set."
auto_inject: false
applicable_when: "Modifying tx admission, leader dequeue, gas accounting, nonce/replay rules, or shielded tx bookkeeping"
confidence: 1.0
verified_at: "09/23/2026"
verified_by: "dsh-agent"
staleness_signal: "If registry fields, used_nonces keying, or dequeue/eviction semantics change"
tags: [registry, transactions, nonce, replay]
edges:
  - target: concept-action-router
    type: related_to
    weight: 0.85
    note: "The router shares one registry (Arc<RwLock>) for nonce/gas coordination across nodes"
  - target: concept-leader-election
    type: related_to
    weight: 0.8
    note: "dequeue_for_leader (registry/pending.rs:221) is what BlockProposer.propose_batch pulls from"
  - target: concept-transaction-lifecycle
    type: related_to
    weight: 0.7
    note: "The registry is the in-memory admission half of the transaction lifecycle"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.6
    note: "used_nonces is append-only and never evicted, so a replayed nonce is permanently rejected"
related: []
source_url: "Empty"
---

# PendingTransactionRegistry: in-flight tx state with replay-proof nonce tracking

`PendingTransactionRegistry` (`src/registry.rs:31`) is the concurrent home of transactions in flight, backed by `DashMap<String, PendingTransaction>` (line 32). It holds six concerns in one object: the `transactions` map; an ordered `TransactionPool` (line 34) for **per-token** leader proposal ordering; `admin_credits` for tax collected at mint (lines 18-28); a `gas_tracker` of gas used per tx, deducted on commit (line 38); `used_nonces` — a **durable, append-only, never-evicted** record of every admitted `(token_id, sender, sequence_number)` triple (lines 39-43); and `shielded_transactions` (lines 44-49), a parallel permanent store for shielded transfers admitted by sentinels (committer hash-matches, finalizer reads at signing).

Admission is race-safe: `add_transaction` (line 70) relies on `DashMap::insert`'s atomic return of the displaced value instead of a prior `contains_key` check, closing the TOCTOU window on concurrent same-id inserts. All methods return `Result` rather than `Option` to distinguish "not found" from "operation failed" (line 14-15).

`dequeue_for_leader` (line 266) pops up to N pending tx ids for a given token from the ordered pool — the entry point the elected leader uses to build a block batch. Nonce keying is per `(token, sender)` because each token has its own account; the registry is therefore also the enforcement point for nonce validation alongside `ActionRouter::check_nonce`.
