---
id: pattern-simultaneous-boot-hides-sequencing-bugs
title: "Simultaneous-boot tests hide sequencing bugs; convergence must be proven with skew and restart"
type: pattern
namespace: pneumatic
visibility: namespace
summary: "Any distributed mechanism whose setup is a one-shot event at boot (announce, handshake, directory pull, seed exchange) passes every same-instant test and fails the first real restart. The Phase 2 rehearsal exposed three instances at once (one-shot announce, first-round-only directory fetch, stale post-restart peer state), each invisible to the loopback suite because it boots peers together."
auto_inject: false
applicable_when: "Designing or reviewing any cluster test, any one-shot boot-time exchange, or any 'it works in docker-compose' claim"
confidence: 0.9
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "If the rehearsal suite has no skewed-boot or restart-and-converge case, this pattern is still active"
tags: [pattern, testing, boot-sequence, convergence, distributed-systems, testnet]
edges:
  - target: fact-rns-periodic-announce
    type: related_to
    weight: 0.9
    note: "Instance one: the one-shot announce that only real skew exposed"
  - target: fact-peering-late-join-convergence
    type: related_to
    weight: 0.9
    note: "Instance two: the one-shot directory fetch, exposed by a container restart"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.8
    note: "Phase 2 exists precisely to run what same-instant tests cannot"
---

# Simultaneous boot hides sequencing bugs

## Shape of the bug

Setup traffic that fires **once**, at T=0, against peers that may not exist yet —
and the only failure-mode message says "retry later", while no code ever retries.
Same-instant tests make T=0 shared, so the receiver is always listening and every
one-shot lands. The first production event that breaks simultaneity — a restart, a
slow host, one dropped UDP packet — strands a node in a permanently half-formed
state that *looks* healthy (routes live, processes up, logs quiet).

## Countermeasures that actually caught the instances

1. **Rehearse with skew, not just with separation.** Distinct namespaces are not
   enough; boot times must differ and at least one node must restart mid-run
   (the phase2c rehearsal formed the mesh; the sentinel restart exposed the
   directory gap).
2. **Make retries state-driven, not round-count-driven.** "Ask until everyone is
   held, then go silent" beats "ask on round 1" — and its steady-state cost is
   zero, so there is no reason to ration it.
3. **Test shape:** boot A, wait a beat, boot B, **no manual glue**, then demand
   both convergence *and* payload delivery. Manual `announce()` calls in tests
   are the loopback equivalent of simultaneous boot.
