---
id: fact-rns-periodic-announce
title: "RNS announce was one-shot at boot; a 10 s re-announce ticker makes routes self-healing"
type: fact
namespace: networking
visibility: namespace
summary: "10/08/2026 (multi-host rehearsal): the transport booted with exactly one announce, so any peer whose listener was not open at that instant (boot skew, restart, lost UDP) held a dead route forever — `route_is_live` only becomes true on a *genuinely received* announce, and the promised 'retry after its announce' could never fire. `RnsNetwork::start` now runs a periodic re-announce ticker (`DEFAULT_ANNOUNCE_INTERVAL` = 10 s); boot order stops mattering, convergence is ~2 intervals. Pinned by `boot_skew_routes_converge_via_periodic_announce` (mutation-verified)."
auto_inject: false
applicable_when: "Diagnosing 'no live route' after a boot that looked clean, restarting any node, or changing announce cadence"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "If the ticker is removed from `RnsNetwork::start`, the interval constant moves (update the ~2-interval claim), or a route goes dead again after formation in a cluster run"
tags: [fact, rns, announce, routes, boot-skew, transport, testnet]
edges:
  - target: fact-control-plane-peering
    type: depends_on
    weight: 0.95
    note: "Peering can only begin after an announce lands; a one-shot announce made that a coin-flip on boot order"
  - target: fact-peering-late-join-convergence
    type: related_to
    weight: 0.9
    note: "The second half of the same late-join story: routes healed, directories still did not"
  - target: task-multihost-testnet-rollout
    type: blocks
    weight: 1.0
    note: "This was the Phase 2 transport blocker: loopback tests booted peers together and hid it"
  - target: pattern-simultaneous-boot-hides-sequencing-bugs
    type: derived_from
    weight: 0.85
    note: "Every loopback test started peers in one instant; the defect only surfaced with per-host skew"
---

# One-shot announce: the Phase 2 transport blocker

## The mechanism of the failure

- `route_is_live(rhash)` is true only for destinations the transport has **received**
  an announce from (`received_at > 0.0`). The pre-seeded bootstrap entries carry
  `received_at: 0` — knowing a peer's address is not having a route to it.
- An announce is a 167 B signed frame; a node sent it exactly **once**, at boot.
- Peers start milliseconds-to-seconds apart; UDP has no handshake. Any listener not
  yet open — or one packet lost — and that peer's route was dead for the process's
  entire life.
- The send path's own error message promised recovery — *"retry after its announce"* —
  but nothing announced again, so the promise was unkeepable.

## Why loopback never caught it

The workspace tests boot both peers in the same instant and often call `announce()`
manually. The first loopback container rehearsal also booted nodes near-simultaneously.
Only four Docker namespaces with real boot skew (plus a container restart mid-run)
reproduced it — evidence: a single 167 B announce on the wire, ephemeral source port,
never repeated (tcpdump), while `Announce:validated` fired only for whoever happened
to be listening at the exact moment.

## The fix

`RnsNetwork::start_with_announce_interval` (plain `start` delegates with
`DEFAULT_ANNOUNCE_INTERVAL = 10 s`): a ticker thread re-calls `node.announce(...)`
every interval; the join handle rides in `workers` so `stop()` shuts it cleanly.
Boot order becomes irrelevant: a late or restarted node converges within ~2
intervals with no operator action.

## Test discipline

`boot_skew_routes_converge_via_periodic_announce` (`src/rns/wrapper.rs`): early node
boots, 400 ms later the late node boots with **no manual announce**; poll until the
early node's route is live (≤10 s), then deliver a 2000 B payload late→early and
assert byte-identical receipt. Mutation-verified: disabling the ticker fails it with
"late->early route never went live".
