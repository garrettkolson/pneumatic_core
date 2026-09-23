---
id: concept-finalizer-role
title: "Finalizer role — quorum checking, block formation, optimistic commit"
type: concept
namespace: pneumatic
visibility: namespace
summary: "The Finalizer = SignatureCollector + BlockBuilder + MessageDispatcher: authenticates executor votes (C1) and commits optimistically on the first authenticated signature; stake-quorum shielded tail."
auto_inject: false
applicable_when: "Working on finalizer/, quorum logic, block building, optimistic finality, or shielded finalization"
confidence: 1.0
verified_at: "09/23/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when finalizer/src/finalizer/ changes its handlers (signing.rs handle_signature / shielded.rs handle_shielded_vote) or the try_finalize variants (finalizing.rs)"
tags: [finalizer, worker-crate, quorum, optimistic-finality, shielded]
edges:
  - target: concept-optimistic-finality
    type: supports
    weight: 0.95
    note: "try_finalize_optimistic (finalizer/finalizing.rs:161-255) is the concrete mechanism of optimistic commit"
  - target: event-s5-2-finalizer-wiring
    type: related_to
    weight: 0.9
    note: "S5.2 added the shielded vote/finalize tail (handle_sign_shielded, try_finalize_shielded) to this role"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.9
    note: "C1 voter authentication and shielded message auth reject fail-closed on any anomaly"
  - target: concept-candidate-registry-conflict
    type: related_to
    weight: 0.7
    note: "Blocks this role finalizes are what later collide in the CandidateRegistry at commit time"
  - target: concept-transaction-lifecycle
    type: related_to
    weight: 0.8
    note: "Moves txs through Finalizing → Committed and carries shielded txs via the recorded-bytes store"
related: []
source_url: "Empty"
---

# Finalizer role — quorum checking, block formation, optimistic commit

The Finalizer orchestrates quorum-checking and block-building (`finalizer/src/finalizer.rs:50-72`; after the 09/23 modularization the per-concern handlers live in `finalizer/src/finalizer/{signing,shielded,finalizing}.rs` and tests under `finalizer/src/finalizer/tests/`), decomposed into three focused components: **SignatureCollector** (collect/verify executor signatures, check quorum), **BlockBuilder** (build `SignedTransaction`/`Block`, sign the finalizer portion), and **MessageDispatcher** (send commits to Committers, clears to Sentinels). Flow: Preload → Sign → (optimistic finalize | quorum) → Clear.

Core paths (all code-verified):

- **`handle_preload`** stores the serialized transaction and acks (`finalizer/src/finalizer/signing.rs:77-95`).
- **`handle_signature`** authenticates the voter (C1: envelope signature + registered `Executor` role), verifies the inner signature over the claimed tx hash, stamps the voter's real stake from the epoch snapshot (never self-reported), adds the signature — then if `signature_count == 1`, immediately calls `try_finalize_optimistic` (`finalizer/src/finalizer/signing.rs:105-163`).
- **`try_finalize_optimistic`** is the fast path: no quorum wait, no reconciliation — one authenticated signature builds the `SignedTransaction` (optimistic variant), signs it, creates an optimistic-finality block chained to the token's chain tip, sends `TransactionCommit` to Committers, gossips `BlockFinalized` with the epoch stake set, clears Sentinels, and transitions to Committed (`finalizer/src/finalizer/finalizing.rs:161-255`).
- **Quorum path**: `try_finalize` reconciles signatures at count-based quorum — exact-integer `u128` math, `sig_count * 100 >= total_voters * quorum` (`signature_collector.rs:76-94`).
- **Shielded tail** (S5.2): `handle_shielded_vote` uses **stake-weighted** quorum (`admitted * 100 >= total_stake * quorum`, deliberately no count-based fast path — `signature_collector.rs:96-127`) and `try_finalize_shielded` rebuilds the tx from recorded canonical bytes, verifies every vote binds the same hash, then dispatches (`finalizer/src/finalizer/shielded.rs:228-258`).
