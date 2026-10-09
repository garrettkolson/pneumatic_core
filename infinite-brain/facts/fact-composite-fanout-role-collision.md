---
id: fact-composite-fanout-role-collision
title: "Per-bucket fan-out loses the target role: a multi-role host routes the finalizer's Preload into the executor adapter"
type: fact
namespace: architecture
visibility: namespace
summary: "10/08/2026, multi-host exit test, final blocker found. The pipeline's wire vocabulary reuses action names across hops: the sentinel preloads raw transactions to the EXECUTOR bucket, and the executor then forwards the STAMPED, executed transaction to the FINALIZER bucket under the SAME action name 'Preload' (executor.rs:867; the 'ordered before Sign' comment at executor.rs:544-547; the finalizer's handle_preload exists specifically to materialize the registry entry the optimistic path loads — signing.rs). On the four-full-node rehearsal every host is in every bucket, and RoleDispatcher admits an action to exactly ONE installed role (two owners = AmbiguousAction fail-closed). 'Preload' is owned by the executor adapter, so the frames meant for finalizers were silently re-ingested by local executors — no error line at all — no finalizer ever got an executable entry, every Sign was rejected TransactionNotInFinalizing (600/node measured), nothing finalized, chain grew 0 of 200. The wire does not carry the fan-out target, so a multi-role host cannot reconstruct which local role should receive. Fix needs a decision (distinct action for the executor→finalizer forward; or dispatch-by-bucket-of-origin on the wire; or content-routing — all touch the wire contract or the dispatcher's single-owner policy)."
auto_inject: false
applicable_when: "Any per-host placement of multiple roles, adding a pipeline action, or changing RoleDispatcher's ownership policy; debugging a multi-host run where hops 'work' in-process but finalize never happens"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "When a disambiguation lands: the rehearsal's finalizers reach executable state from wire traffic and the chain grows end-to-end; also if RoleDispatcher ever gains multi-owner delivery (then re-audit every reused action name, not just Preload)"
tags: [fact, composite, role-dispatcher, fanout, preload, multihost, per-host-placement]
edges:
  - target: fact-composite-action-set-drift
    type: related_to
    weight: 0.9
    note: "Same admission layer, harder class: that was a narrow list; this is a list that CANNOT admit one action to two owners"
  - target: concept-node-server-composite-runtime
    type: depends_on
    weight: 0.95
    note: "RoleDispatcher's single-owner policy is the mechanism"
  - target: concept-sentinel-role
    type: related_to
    weight: 0.8
    note: "The Preload→Sign→finalize hop whose wire vocabulary collides"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.95
    note: "The last known blocker of Phase 2's exit test — needs a design call"
---

# A message addressed to two roles arrives addressed to none

## The hop chain on the wire

```
sentinel --"Preload"(raw tx)------> Executor bucket
executor --"Preload"(STAMPED tx)---> Finalizer bucket    <-- same action, different hop
executor --"Sign"-----------------> Finalizer bucket
finalizer--"Commit"/"BlockFinalized"-> Committer bucket
```

The finalizer's `handle_preload` (finalizer/signing.rs) documents why it must
exist: "the optimistic-finality path loads the transaction from
`self.pending_registry` and fails closed if the entry is missing … entry must
be in an executable state". In the single-composite e2e this never bites: the
one host's **one** shared `PendingTransactionRegistry` is populated by the
local sentinel role, so the finalizer's entry is already there regardless of
which adapter swallows the wire Preload. **The tests made the design look
right while the design was unimplementable across hosts.**

## What the four-host mesh measured

- zero "unknown action: Preload" lines — because the action HAS an owner;
- the executor adapter (`ingest_preload`) happily re-registered the
  executor-stamped transaction **on the finalizer node as an executor entry**;
- finalizer registries stayed empty of executable entries;
- every `Sign` rejected `TransactionNotInFinalizing` (600/node), every
  late `Commit` failed H12-adjacent state gates — and the chain grew 0 of 200
  with a perfectly healthy mesh (peers 12/12, zero panics, all counters live).

## The decision ahead (three shapes, all touch contracts)

**RESOLVED the same night, 10/08/2026 — option 1 (distinct action names).** The
executor→finalizer forward now rides a Finalizer-OWNED `"PreloadForFinalizer"`
admitted in `FINALIZER_ACTIONS`; the sentinel→executor hop keeps `"Preload"`, and
`RoleDispatcher`'s single-owner policy is unchanged — a pairwise-disjointness test
fails loudly if any two roles ever share a name again. The 8-node real-RNS pipeline
e2e is the "cannot be faked by one composite" pin this section asked for. What the
fix did NOT resolve is the blocker it exposed: the fan-out still had no local
member to reach, [[fact-composite-no-self-delivery]]. The three shapes below are
kept as written, because the reasoning is the reusable part.

1. **Distinct action names per hop** (e.g. the executor's forward becomes
   `"ExecutedPreload"` owned by the finalizer) — smallest blast radius;
   changes the wire contract the standalone crates speak; pipeline tests pin
   action strings.
2. **Fan-out target on the wire**: `send_to_all` stamps the destination role
   into the frame; the bridge routes by (target-role, action). Honest but
   touches every sender/receiver and the RNS payload shape.
3. **Multi-owner delivery** in `RoleDispatcher` (drop AmbiguousAction,
   deliver to all owners) — WRONG for the sentinel hop: raw Preload would
   also run on finalizers; content-routing ("stamped ⇒ finalizer") then
   becomes mandatory and implicit. Rejected on inspection: hidden coupling.

Whatever lands must be pinned by a test that **cannot** pass in a single
composite — the shape: two host-processes, executor on one, finalizer on the
other, assert the finalizer's registry reaches executable state **from the
wire alone**. (Closest existing harness: `rns_live`/peered-topology tests.)
