---
id: concept-telemetry-health-metrics
title: "Telemetry layer: tracing + Prometheus-text metrics + HTTP health, with graceful-shutdown ordering"
type: concept
namespace: pneumatic
visibility: namespace
summary: "src/telemetry.rs is the ops surface: init_tracing (RUST_LOG), Metrics (atomic counters/gauges → Prometheus text), HealthState (200→503 draining), a hand-rolled minimal HTTP server over tokio, and wait_for_shutdown_signal (Ctrl-C+SIGTERM). Binaries drain in a fixed order; no consensus hot path is instrumented."
auto_inject: false
applicable_when: "Adding metrics, extending the health endpoint, or wiring graceful shutdown into a new binary"
confidence: 0.95
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale if an HTTP framework replaces the hand-rolled responder, metrics move onto consensus hot paths, or the Logger-trait stream is bridged into tracing"
tags: [observability, telemetry, metrics, health, shutdown, phase-8]
edges:
  - target: event-phase8-production-readiness
    type: related_to
    weight: 0.9
    note: "Introduced by the Phase 8 close"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.6
    note: "Health-bind and data-address failures are fail-soft BY design — the contrast is deliberate: ops affordances never gate consensus boot"
  - target: concept-node-server-composite-runtime
    type: depends_on
    weight: 0.7
    note: "The node-server binary drives build_runtime + poll_and_advance + NodeServer::shutdown through this layer's signal helper"
related: []
source_url: "Empty"
---

# Telemetry layer (Phase 8)

**`src/telemetry.rs`** is the operator-facing surface; consensus code is untouched
by it. Design decisions with rationale:

- **No HTTP crate.** `spawn_health_server` (`telemetry.rs:287`) parses only the
  request line of GETs, bounds the head at 8 KB, times out reads at 5s, and
  always answers `Connection: close` — the exact shape of a healthcheck/scraper,
  nothing more. `spawn_accept_loop` is split so tests bind an ephemeral port
  without a rebind race.
- **Metrics are atomics.** `Metrics` (`telemetry.rs:72`): DashMap of
  `AtomicU64` (separate counters/gauges maps), `increment`/`set_gauge`/`render`;
  render sorts names for stable Prometheus text. Pollers publish read-only
  depths (epoch, peers, pending/shielded counts, tokens, roles) — deliberately
  **no hot-path counters yet**; message/block counters are future work.
- **Draining, not just dying.** `HealthState::mark_stopping()` flips `/health`
  to 503 *before* any drain work. Fixed binary shutdown order:
  mark_stopping → watch-channel flip (loops exit immediately, not after the
  interval) → `StakeIndex::stop()` (committer) / `NodeServer::shutdown()`
  role fan-out (node-server) → 2s grace → exit. Inside docker's 10s SIGKILL
  grace. RNS workers are process-scoped and die with it (`RnsNetwork::stop(self)`
  is unreachable once handlers hold Arc clones — documented trade-off).
- **`Logger` (logging.rs) is not bridged** into tracing: the file logger is the
  durable consensus-side stream; stdout tracing is the operator stream. Two
  channels by design.

Env surface (runbook §2.2): `PNEUMATIC_HEALTH_ADDR`, `PNEUMATIC_DATA_ADDR`,
`PNEUMATIC_EPOCH_INTERVAL_MS`, `RUST_LOG`.
