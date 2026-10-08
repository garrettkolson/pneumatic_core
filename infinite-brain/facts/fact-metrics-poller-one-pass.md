---
id: fact-metrics-poller-one-pass
title: "`watch::changed()` is not a timer: the metrics poller that polled exactly once"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/08/2026 rehearsal: every node exported `pneumatic_node_peers 0` while its fragments proved the directories full. The inlined poller loop ended each pass with `shutdown_rx.changed().await` — a tokio watch receiver resolves that only on send/drop, i.e. at shutdown — so the loop body ran once at boot and parked forever. Metrics that freeze at boot values are worse than absent: they advertise a healthy-looking lie indefinitely. Fixed by moving the poller into `node_server::metrics_poller` as `spawn_metrics_poller(server, metrics, shutdown, interval)` with a `tokio::select!` sleep arm; `metrics_poller_refreshes_gauges_after_boot` pins it (mutation-verified)."
auto_inject: false
applicable_when: "Writing any loop that must refresh on a cadence while also honoring shutdown, or reading any gauge that looks suspiciously constant"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "If a poller loop appears that awaits only `changed()`/`recv()` with no timer arm, or if a dashboard shows a gauge pinned at its boot value"
tags: [fact, metrics, tokio, watch, poller, monitoring, bug]
edges:
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.85
    note: "Phase 2's resource numbers would have been collected through frozen gauges"
  - target: fact-control-plane-peering
    type: related_to
    weight: 0.7
    note: "The gauge that lied described the peering state that was actually healthy"
---

# The poller that polled once

## The trap

```rust
loop {
    set_gauges();
    if shutdown_rx.changed().await.is_err() { break; }
}
```

`tokio::sync::watch::Receiver::changed()` resolves when the **value is sent** (or the
sender drops). Nobody sends during normal operation → the first await parks forever →
the gauge pass runs exactly once. The pattern reads like "wait for the next tick or
shutdown", but a watch channel carries *state changes*, not time.

## The fix shape (the one that is correct for shutdown-aware cadence loops)

```rust
tokio::select! {
    biased;
    r = shutdown.changed() => { if r.is_err() || *shutdown.borrow_and_update() { break; } }
    _ = tokio::time::sleep(interval) => {}
}
```

`biased` keeps shutdown first so a fired shutdown is never starved by a due timer.

## Why the test registers the peer AFTER spawn

`metrics_poller_refreshes_gauges_after_boot` (node-server): spawn the poller, confirm
the boot-time gauge (0 peers), then register a peer and wait for the gauge to move.
Registering *before* spawn would also pass under the old bug (the one pass would see
it) — the ordering is the whole test. Mutation-verified by deleting the sleep arm:
the test fails with the rehearsal message.
