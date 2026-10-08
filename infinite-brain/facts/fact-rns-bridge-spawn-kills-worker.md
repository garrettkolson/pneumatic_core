---
id: fact-rns-bridge-spawn-kills-worker
title: "The composite's RNS bridge spawned into no reactor: one data frame killed a worker and evicted the whole mesh"
type: fact
namespace: networking
visibility: namespace
summary: "10/08/2026, first cross-host data-plane run: every node's data-plane bridge (build.rs on_packet) dispatched with bare `tokio::spawn` — but the RNS `on_packet` callback runs on the wrapper's own inbound worker threads, plain `std::thread`s with no ambient reactor. The first real data-plane frame (a 200-tx burst) panicked one worker per receiving peer ('there is no reactor running'); the panic dropped the worker's inbound queue receiver, every later frame logged 'inbound queue closed; dropping', no heartbeats were processed, and within one 30 s eviction window every directory emptied itself — formed mesh, `pneumatic_node_peers` 12 → 0, chain grew by 0 of 200 while every health endpoint stayed green. Fixed: `spawn_data_dispatch` spawns through a `Handle` captured inside the host runtime at build time (correct from any thread), mutation-verified by calling it from a bare std thread. Every prior test drove the bridge from inside a runtime (e2e, ingress) — production's calling context had never existed in tests."
auto_inject: false
applicable_when: "Writing any callback that RNS invokes (on_packet/on_announce), debugging a mesh that collapses when traffic starts, or seeing 'inbound queue closed; dropping'"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "If any bridge callback regains a bare `tokio::spawn`/`Handle::current()` call-site (the regression test fails first); if rns-net ever delivers packets from inside a runtime"
tags: [fact, rns, tokio, panic, worker-threads, data-plane, composite, multihost]
edges:
  - target: concept-node-server-composite-runtime
    type: depends_on
    weight: 0.95
    note: "The bridge lives in the composite runtime's build — the callback wiring it installs"
  - target: concept-rns-transport
    type: depends_on
    weight: 0.9
    note: "The wrapper's inbound worker threads are std threads; that threading contract is the fact"
  - target: pattern-simultaneous-boot-hides-sequencing-bugs
    type: related_to
    weight: 0.85
    note: "Same family of hidden-context bug: tests ran the bridge inside a richer context than production's"
  - target: fact-mesh-verification-probe
    type: related_to
    weight: 0.8
    note: "The eviction emptied signed directories after the fact; health endpoints never noticed"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.9
    note: "Found by Phase 2's first sustained-traffic attempt across namespaces"
---

# One data frame = one dead worker = one dead mesh

## The scene

First-ever cross-host data-plane run (Phase 2, `traffic.sh 200 100`). Control
plane was textbook-perfect: probe 12/12 exit 0, all peers gauges 12, counters
climbing. 20 s into the tx burst every node began logging:

```
thread '<unnamed>' panicked at node-server/src/node_server/build.rs:267:25:
there is no reactor running, must be called from the context of a Tokio 1.x runtime
[pneumatic] rns: inbound queue closed; dropping resource      ← forever after
```

and `pneumatic_node_peers` marched 12 → 0 on every node (liveness eviction,
since no worker remained to process heartbeats). Chain grew by 0 of 200. The
senders never saw anything wrong.

## The mechanism, in order

1. **RNS delivers inbound packets on its own worker threads** — plain
   `std::thread::spawn` in the wrapper's `start`, no tokio anywhere in the
   wrapper. The composite's `on_packet` callback therefore runs with **no
   ambient reactor**.
2. The data-plane branch ended with `tokio::spawn(async move {
   route_data_plane(...) })` — legal to compile, **panic at call**: control
   frames (register/directory) never reached this line, so a formation-only
   rehearsal can never hit it. The first pipeline `Message` did.
3. The panic kills the worker **thread**; its inbound channel's receiver
   drops → every subsequent frame from rns-net logs
   `inbound queue closed; dropping` — the node goes deaf permanently.
4. Deaf means unprocessed peer heartbeats → after the 30 s liveness cutoff
   the eviction loop drains every directory: `peers` 12 → 0. The health
   endpoint (a separate tokio task) keeps answering 200 throughout.

## Why every test missed it

Every prior consumer ran the bridge **from inside a runtime** — e2e tests call
`route_data_plane` directly, `#[tokio::test]`/`#[tokio::main]` provide the
context production's worker threads never have. The real calling context
(bare std thread) existed in **zero** tests. Regression now pins it:
`bridge_data_dispatch_survives_a_runtimeless_caller` invokes the fixed helper
from a bare `std::thread` and fails at the join with the rehearsal's own
panic message if it regresses to `tokio::spawn` (mutation-verified).

## The rule

Anything RNS invokes (`on_packet`, `on_announce` consumers) runs on wrapper
threads. **Capture a `tokio::runtime::Handle` where a runtime exists (build
time, inside the host runtime) and spawn through it.** Never depend on
ambient context at callback time.

## Sibling gap found en route (open)

`send_to_all`'s RNS branch discards the send Result (`let _ =` —
fanout.rs) while the direct branch records failures — so the failed data
sends were invisible on the SENDER side too. Phase 6.2's "every lost send is
observable" promise holds only for the direct branch; fix queued with the
Phase 3 harness work.
