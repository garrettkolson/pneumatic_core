---
id: fact-peering-late-join-convergence
title: "A late-joining node healed its routes but never its directories: catch-up requests fix it"
type: fact
namespace: networking
visibility: namespace
summary: "10/08/2026 rehearsal, caught by restarting one container: with periodic announce (fact-rns-periodic-announce) a late node validates peer announces and holds live routes within seconds, yet kept `pneumatic_node_peers 0` forever — peering re-sends Register each tick but requested directories only on its first round, those early requests died on 'no live route', and peer Register acks carry only the acking peer's own entry. Fixed with `catch_up_directories_from_bootstrap_peers`: every tick, re-ask exactly the bootstrap peers not yet held, silent once complete. Same rehearsal also exposed self-present — a directory response names the reader back at itself, now filtered on receive. LIMIT (proven same night): the catch-up request is necessary but NOT sufficient for restart — long-running peers' replies die in stale cached session state, so a restarted single node still never converges (see fact-rns-stale-session-state-blocks-restart); the fix here fully explains and repairs the pure late-join (never-booted) case."
auto_inject: false
applicable_when: "Restarting any node, debugging a node with routes but no peers, or changing the peering loop's round structure"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "If the peering loop drops the catch-up call, if `bootstrap_peers_needing_directory` stops comparing like-for-like rhashes, or if a never-booted late joiner with live routes fails to reach full peers within ~2 ticks (restart cases are governed by fact-rns-stale-session-state-blocks-restart until that lands)"
tags: [fact, peering, directory, late-join, restart, self-present, control-plane]
edges:
  - target: fact-control-plane-peering
    type: depends_on
    weight: 0.95
    note: "The loop whose first-round-only directory fetch caused the stall"
  - target: fact-rns-periodic-announce
    type: related_to
    weight: 0.95
    note: "Fixed half the problem: routes heal, and this fact is about the half that still did not"
  - target: pattern-simultaneous-boot-hides-sequencing-bugs
    type: instance_of
    weight: 0.9
    note: "The one-shot pattern again — here the fetch was round-one-only"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.85
    note: "Phase 2's restart case is what a single-host loopback run never exercises"
  - target: fact-rns-stale-session-state-blocks-restart
    type: depends_on
    weight: 0.95
    note: "For restart to actually converge, the response leg needs this fact's fix too"
---

# Late join: routes healed, directories never did

## The symptom that pinned it

A restarted container with debug logging showed:

- `Announce:validated dest=… hops=1` for **all three** peers, every 10 s — transport healthy;
- Register failures only from before the first route-up, none after (retries landed);
- `pneumatic_node_peers 0` **forever**, and a fragment with empty buckets.

## Why

Registration is pairwise: it teaches the *receiver* about the *sender*, and the ack
travels back carrying just the acking peer. **Directories — the map of everyone —
only arrive via directory responses.** The peering loop requested them once, on its
first round, at a moment when no route was live yet: every request died on
`no live route … retry after its announce` — and nothing ever requested again. The
phrase named the recovery; no code kept the promise.

## The fix — state-driven, not round-driven

`catch_up_directories_from_bootstrap_peers` runs **every tick**: asks exactly those
bootstrap peers whose rhash is not held in any of our buckets. Termination is by
state ("everyone known"), so a healthy steady state sends **zero** directory traffic;
a late joiner completes within a tick or two of its routes going live. The held-test
compares the same rhash derivation registration stores, so learning a peer anywhere
takes it off the list (`catch_up_asks_exactly_the_bootstrap_peers_not_yet_learned`,
mutation-verified).

## Self-present: the same rehearsal's second finding

Registration puts our entry in every peer's buckets, so **every directory response
names the reader back at itself**. Installing that planted our own rhash in our own
directories — what mesh-probe correctly flagged as `self-present` on all four nodes.
Receiver-side fix in `handle_directory_response`: skip entries matching our rhash or
public key (`a_directory_response_naming_ourselves_never_lands_in_our_own_directories`).
The responder cannot filter it — it does not know who asks.

## The limit of this fix (same-night follow-up)

With catch-up shipped, the restarted node still never converged — and the
instrumented rehearsal proved the **request side is exactly what this node
describes as fixed**: announces validated, registers sent, catch-up requests
sent every tick, no errors. The failure moved downstream: the long-running
peers process the incoming registers (their fragments prove it — entry fresh
and vouched) but their replies die in cached session state belonging to the
restarted node's dead instance. See
`fact-rns-stale-session-state-blocks-restart` for the measurement, mechanism,
proof-by-resurrection, and the designed fix. This node's own repair is exactly
correct and load-bearing for what it covers — pure **late join** (a node whose
first-ever boot happens after the mesh exists, where peers hold no stale state
about it): there, the directory responses are first-time links and land.
