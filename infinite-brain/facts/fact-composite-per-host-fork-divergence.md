---
id: fact-composite-per-host-fork-divergence
title: "The per-tx finalizer selection is never applied on the primary path — every Finalizer the transaction reaches finalizes it, so N finalizers make N blocks"
type: fact
namespace: architecture
visibility: namespace
summary: "10/08/2026, surfaced the moment self-delivery landed and commits started landing — and NOT a consequence of the design. ADR-004 (`decision-deterministic-per-tx-routing`) says each transaction routes to ITS OWN finalizer via `deterministic_select(stakers, tx_id, epoch)`; one transaction per block, so a slot has exactly one producer. On the wire nothing honors that: the sentinel's standard path enqueues with `finalizer_public_key: vec![]` (processing.rs:138-146), the executor reads that empty key back (executor.rs:804-816) and writes it into Finalizing (:538) before broadcasting the finalizer-bound hop to the WHOLE bucket — `send_to_all(PreloadForFinalizer, Finalizer)` (executor.rs:882, :919); `TransactionNotifier::send_to_finalizer_for_preload` takes a `finalizer_key` parameter and ignores it (`transaction_notifier.rs:95-116`, same at :129-147); the finalizer never checks whether it was the one chosen, reading the (empty) key only to re-store it (finalizer/finalizing.rs:148-159, 177-180); and `set_requested_finalizer` has no production caller, so the sentinel's \"Confirmation from unassigned finalizer\" gate (`finalizing.rs:22`) can never pass on the primary path. `deterministic_select` has exactly ONE production caller — `reassign_finalizer_deterministically` (finalizing.rs:257), i.e. selection exists only for the rejection path. So every host running a Finalizer builds and signs its own block for the same tx (hash binds the build timestamp), and each host's Committer accepts the copy matching its own local booking (P10 `wire_authoritative`) while refusing the rest."
auto_inject: false
applicable_when: "Fixing finalizer assignment or the finalizer-bound hop; adding any send to the Finalizer bucket; reasoning about why committed chains differ per host; reading per-node COMMIT-OK/acquire_fail/ChainLinkage tallies; touching `is_requested_finalizer` or the reassignment path"
confidence: 0.95
verified_at: "10/09/2026"
verified_by: "dsh-agent"
staleness_signal: "RESOLVED on the primary path 10/09/2026 — re-verify the table below against the code before repeating it. The five-step fix shape at the bottom is now implemented, with an extra step it did not anticipate: the decision must belong to the TRANSACTION (a second Sentinel that re-attests re-opens this exact fault), and the committer must compare the incoming decision against the one its own registry holds. Re-open this node if a finalizer-bound hop ever goes back to a bucket broadcast, if a Sentinel overwrites a routing record that arrived with a transaction, or if COMMIT-OK counts again exceed the transaction count."
tags: [fact, finalizer, selection, optimistic-finality, fanout, composite, committer, multihost, per-host-placement]
edges:
  - target: decision-deterministic-per-tx-routing
    type: contradicts
    weight: 0.95
    note: "ADR-004 specifies one finalizer per tx via deterministic_select; this is the primary path never applying it"
  - target: fact-composite-no-self-delivery
    type: depends_on
    weight: 0.9
    note: "Self-delivery had to land first: until a host could commit at all, the N-producers consequence was invisible behind 'chain 0 of N'"
  - target: decision-conflict-definition-and-resolution
    type: related_to
    weight: 0.9
    note: "The winner/loser rollback (`rollback_tip_hash`) is what lets one of those N blocks replace an already-committed local tip"
  - target: decision-optimistic-finality
    type: related_to
    weight: 0.8
    note: "The optimistic path books Committed{own block hash} at finalize time, which is why each host accepts its own copy and only its own"
  - target: fact-composite-fanout-role-collision
    type: related_to
    weight: 0.7
    note: "Same class of bug one layer down: role-bucket fan-out cannot express a per-key destination, so a per-tx choice has nowhere to ride"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.95
    note: "Phase 2's open data-plane blocker once chain growth became possible"
  - target: fact-h12-assumes-lockstep-execution
    type: related_to
    weight: 0.95
    note: "What replaced this fault once routing landed: one producer per tx, and committers that refuse each other's blocks"
  - target: decision-deterministic-per-tx-routing
    type: resolves
    weight: 0.9
    note: "The ADR the fix implemented; the node's `contradicts` edge is now historical"
---

# Nobody is the chosen finalizer, so everybody finalizes

> **RESOLVED on the primary path, 10/09/2026** (`3c0d867`, `89584e5`). Selection runs on
> the standard path and is attested; the finalizer hop goes to one key; the Finalizer
> gates preload, vote intake and the finalize choke point; the Committer refuses a block
> whose routing decision is not the one it holds. Measured change on the four-container
> rehearsal: before — committer-1 appended **two** blocks with two different hashes for
> one transaction while every host built its own; after — exactly one host produces and
> commits, and the other three refuse instead of booking their own. The tables below are
> kept as the record of what the code did, because they are the diagnosis that produced
> the fix; treat them as historical and verify against the tree before quoting them.
> **What this did NOT fix is convergence** — with one producer, the receivers refuse the
> producer's block on the H12 payload compare:
> [[fact-h12-assumes-lockstep-execution]].

## What the design says

ADR-004: each transaction routes to its own finalizer via
`deterministic_select(stakers, seed_bytes = tx_id_bytes, epoch_number)` over the
stake snapshot frozen at the epoch boundary. Blocks carry one transaction, so a
transaction has exactly one producer and honest nodes — reading the same frozen
snapshot — agree on which one.

## What the code does on the standard path

| Step | Site | What happens to the assignment |
|---|---|---|
| Sentinel validates & enqueues | `sentinel/src/sentinel/processing.rs:138-146` | `TransactionValidationResult { finalizer_public_key: vec![], .. }` — empty from the start |
| Executor reads it back | `executor/src/executor.rs:804-816` | `get_finalizer_key` returns that `vec![]` |
| Executor records it | `executor/src/executor.rs:538` | `transition_to_finalizing(tx, vec![])` |
| Executor sends the hop | `executor/src/executor.rs:882`, `:919` | `send_to_all(PreloadForFinalizer, Finalizer)` — **every** finalizer |
| Sentinel's own notifier | `sentinel/src/transaction_notifier.rs:95-116` | signature takes `finalizer_key: &[u8]`, body calls `send_to_nodes(Finalizer, …)` → `send_to_all` (:240-254). The parameter is decorative; same at `:129-147` |
| Finalizer finalizes | `finalizer/src/finalizer/finalizing.rs:148-159`, `:177-180` | reads the (empty) key to re-store it; no comparison against its own identity |
| Would-be gate | `sentinel/src/sentinel/finalizing.rs:22` | `is_requested_finalizer` — needs `Finalizing{finalizer_key}`, which nothing on the primary path ever sets (`set_requested_finalizer` is test-only) |

`deterministic_select` for finalizers is called in exactly one place,
`reassign_finalizer_deterministically` (`sentinel/src/sentinel/finalizing.rs:257`)
— the **rejection** path. Selection exists as a recovery mechanism and never as
the primary one.

## The measured consequence

4 hosts × 4 roles, 15 transactions. Per node: `COMMIT-OK` 15–18 (≥ one per tx),
`acquire_fail` 28–71, `ChainLinkage` rejections 0–34. For `ph2-926` on
committer-1 the arriving `Commit` copies carried at least three distinct block
hashes — three different producers for one transaction. Each host accepted the
copy whose hash matched its own optimistic booking (the P10 `wire_authoritative`
branch, `committer/src/committer/committing.rs:91-116`) and refused the others.

Worse than divergence: `Token::commit_block`'s `rollback_tip_hash` lets a peer's
block **replace** an already-committed local tip, so one host can commit the same
transaction into two different blocks, and `Token::sequence_number` — the
rehearsal's delivery counter — counts appends rather than transactions
([[fact-token-chain-window-caps-delivery-gate]]).

No double-*delivery* is involved: no transaction was committed twice with the same
block hash on any node, which is what duplicating a fan-out would leave behind.

## The fix shape (all machinery already exists)

1. **Select on the primary path**, before the finalizer-bound hop:
   `deterministic_select` over the frozen snapshot with the ADR-019 chain-tip salt
   — the same call the reassignment path already makes (`finalizing.rs:257`).
2. **Record it** so the gate becomes real: `transition_to_finalizing(tx, key)` /
   `set_requested_finalizer` on the primary path, and put the key in the
   validation result the executor reads back instead of `vec![]`.
3. **Deliver to that one key**: `request_single_finalizer`
   (`transaction_notifier.rs:201`) is already implemented and keyed — it is used
   only by reassignment today. Make the `finalizer_key` parameter in
   `send_to_finalizer_for_preload`/`send_sign_shielded` load-bearing instead of
   discarded.
4. **Enforce at the receiver**: a Finalizer refuses a finalize request naming a
   different finalizer. Today it cannot, because it never looks.
5. **Then the committer** should refuse a block from a non-selected finalizer —
   role-level authentication ("a Finalizer signed this") is not
   identity-level ("the selected Finalizer signed this").

Under per-host placement, all five matter: a bucket fan-out plus an unchecked
receiver turns every host into a producer regardless of what was selected.
