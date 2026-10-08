---
id: fact-rns-stale-session-state-blocks-restart
title: "A restarted node is permanently one-way: long-running peers keep replying to its dead session"
type: fact
namespace: networking
visibility: namespace
summary: "Measured 10/08/2026 (4-container rehearsal, instrumented image): after a single-node restart, the restarted side is flawless — peer announces validated within seconds, register + catch-up directory requests all sent without error — yet it receives NOTHING back (`verifications_total` 0, peers 0, forever) while the long-running peers do process its registers (their fragments show the entry fresh, vouched, last_seen 0 s). The replies die on the RETURN leg in state the long-running peers cache per destination from the dead instance: the wrapper's link_id cache (resource path) and rns-net's ratchet store (direct path — announce carries no ratchet for the restart, so the stale one is never replaced; its frames arrive undecryptable — that is the `failed to decrypt inbound packet` flood at the restarted node). Proof by resurrection: restarting the peer itself unblocks the restarted node within seconds (peers 0→8 immediately). Until receivers invalidate per-destination session state on re-announce, single-node restart is unsupported."
auto_inject: false
applicable_when: "Any restart/persistence work (Phase 3), debugging a node at 0 peers after a bounce, or touching the wrapper's link cache or announce payloads"
confidence: 0.9
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "When session-state invalidation on re-announce lands (then rewrite as history) — the observable: a restarted container reaches full peers within ~2 ticks on its own; also if rns-net starts refreshing its ratchet store from ratchet-less announces"
tags: [fact, rns, restart, session-state, link-cache, ratchet, control-plane]
edges:
  - target: fact-peering-late-join-convergence
    type: related_to
    weight: 0.95
    note: "Catch-up fixes the request side; this blocks the response side — restart needs BOTH"
  - target: fact-rns-periodic-announce
    type: related_to
    weight: 0.9
    note: "The 10 s re-announce keeps telling peers who we are — but carries no new session material to replace theirs"
  - target: fact-control-plane-peering
    type: depends_on
    weight: 0.85
    note: "Registers/acks/directories are the traffic this state silently drops"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.9
    note: "Phase 3 (persistence & restart) opens with this fix; Phase 2 exit test deliberately uses full re-up"
---

# Restart is one-way until session state is invalidated

## What was measured (not inferred)

An instrumented boot (`on_announce` logging the announce's public key and the
derived rhash) on the 10/08/2026 docker rehearsal, restarting **one** container:

| observable | restarted node | long-running peer |
|---|---|---|
| peer announces | validated every ~10 s, derived rhash **equals manifest rhash** (`known=true`) | — |
| Register sends | succeed (zero `no live route` after route-up) | processes them: fragment entry **fresh, vouched, last_seen 0 s** |
| catch-up directory requests | sent every tick, no errors | never get a visible reply back |
| inbound protocol frames | **`pneumatic_signature_verifications_total` = 0, forever** | keeps the restarted peer in all four buckets |
| `failed to decrypt inbound packet; dropping` | floods at ~peer-heartbeat cadence | normal |

So the forward leg (restarted → peer) is end-to-end correct; **every return
frame vanishes**. The restarted process holds the same persistent keys
(keystore unchanged) — a healthy sender would decrypt them.

## Why: two per-destination caches of the DEAD instance

1. **Wrapper `links: DashMap<DestHash, LinkId>`** (resource path — all big
   control frames: RegisterAck ~1.9 KB, DirectoryResponse ~6 KB, and all data
   plane). A link is a *session* object: the old peer established it against
   the previous instance and reuses the cached id forever
   ("create_link creates a new link each call, so don't re-create"). Writes
   into it never reach the new instance.
2. **rns-net's `ratchet_store` keyed by destination_hash** (direct path —
   small frames, ≤481 B). It updates **only when an announce carries a
   ratchet** (`if let (Some(store), Some(ratchet))`). Our announces carry
   none, so a restart never refreshes it: peers keep encrypting direct
   packets to the dead instance's ratchet — the restarted node receives them
   and fails to decrypt — **that is the persistent decrypt flood**.

## Proof by resurrection

Restarting the *peer as well* (finalizer, 10/08/2026 ~18:05) rebuilt its links
and ratchet fresh, and the stuck sentinel's gauge jumped **0→4→8 peers within
seconds** while it had sat at 0 for ten minutes. Nothing about the restarted
node changed; only the peer's cached state did.

## Fix sketch (designed, not yet built — Phase 3)

- Announce carries a **per-boot random seq** in `app_data` (rns-net supports
  `app_data` on announce; the wrapper's `on_announce` already receives it).
  Receiver compares seq per rhash; on change, **drop the cached link**
  (`links.retain(|_, l| *l != stale)`) so the next send re-establishes.
  `on_announce` is the correct hook: it already runs per announce with the
  authoritative identity, and the 10 s ticker bounds detection latency.
- For the direct path: either attach a fresh ratchet to the announce (rns-net
  refreshes its store) or stop using the ratchet path for direct frames.
  Verify by drill: `docker rm -f` one node, expect full peers within ~2 ticks
  WITHOUT touching anyone else.
- Mutation-test: stale link is actually dropped; fresh announce (same seq)
  does NOT churn links.

## Interim operational rule

Single-node restart is **unsupported** on this stack; recovery = full re-up
(`./down.sh && ./up.sh`) — documented in `deploy/multihost/RUNBOOK.md`.
