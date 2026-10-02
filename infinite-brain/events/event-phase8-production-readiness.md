---
id: event-phase8-production-readiness
title: "Phase 8: production readiness landed (telemetry, graceful shutdown, Docker, runbook) — 10/01/2026"
type: event
namespace: pneumatic
visibility: namespace
summary: "Phase 8 closes the last planned roadmap phase: pneumatic_core::telemetry (tracing init, Prometheus-text metrics, HTTP health), SIGINT/SIGTERM graceful shutdown in both binaries, the real node-server composite binary, Dockerfile + compose + guard-tested example configs, the operator runbook, and a rustdoc pass; workspace 997/0/37."
auto_inject: false
applicable_when: "Answering what Phase 8 delivered or where observability/deploy artifacts live"
confidence: 0.95
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Superseded if the telemetry/deploy surface is restructured (e.g. metrics moved onto hot paths, or a non-Compose deploy guide replaces the runbook)"
tags: [event, phase-8, production-readiness, observability, deployment, shutdown]
edges:
  - target: event-p10-e2e-determinism
    type: preceded_by
    weight: 0.9
    note: "Phase 8 landed the same day the executor contract-execution plan closed (P10 was the preceding milestone)"
  - target: concept-telemetry-health-metrics
    type: related_to
    weight: 0.9
    note: "The telemetry module is the durable architectural artifact this event introduced"
  - target: note-roadmap-status-2026-10-01-phase8
    type: followed_by
    weight: 0.9
    note: "The dated status snapshot recording Phase 8 completion"
  - target: hyp-readme-postmvp-phase-8-outstanding
    type: related_to
    weight: 0.8
    note: "This event resolves the hypothesis (Phase 8 items were outstanding planned work)"
related: []
source_url: "Empty"
---

# Event: Phase 8 production readiness landed — 10/01/2026

What landed (all comment-consistent with the code at this commit):

1. **`pneumatic_core::telemetry`** (new module, `src/telemetry.rs`): `init_tracing`
   (RUST_LOG env-filter, stdout fmt), `Metrics` (DashMap of `AtomicU64` counters/
   gauges → sorted Prometheus text), `HealthState` (`/health` 200→503 on
   `mark_stopping`), `spawn_health_server` (hand-rolled minimal HTTP/1.1 over
   tokio — GET-only, 8 KB head bound, 5s read timeout, Connection: close),
   `wait_for_shutdown_signal` (Ctrl-C + SIGTERM). 7 unit tests.
2. **Graceful shutdown in both binaries**: a `tokio::sync::watch` channel stops
   the epoch loop (committer) / coordinator + metrics poller (node-server);
   ordering = mark_stopping → flip channel → stop StakeIndex / `NodeServer::shutdown()`
   fan-out (`initiate_all_shutdown`) → 2s grace → exit. No consensus-code changes.
3. **Real `node-server` binary** (was a `println!("scaffold")` stub): Config::build →
   `build_runtime` → coordinator tick loop (`poll_and_advance`,
   `PNEUMATIC_EPOCH_INTERVAL_MS` default 5000) → health/metrics → signal drain.
   Includes `DataStakeProvider` (own-stake reads from the data service, fail-closed 0).
4. **Deploy infra**: root `Dockerfile` (rust:1.87-slim multi-stage, non-root,
   HEALTHCHECK curl on :9500), `deploy/docker-compose.yml` (committer + full-node,
   shared image), `deploy/config/**` examples guarded by `tests/deploy_examples.rs`
   (schema drift fails the test, not `docker compose up`), `.dockerignore`.
5. **Env-var surface**: `PNEUMATIC_HEALTH_ADDR` (default 127.0.0.1:9500),
   `PNEUMATIC_DATA_ADDR` (remote data service via `ConnTarget::Remote`),
   `PNEUMATIC_EPOCH_INTERVAL_MS`, `RUST_LOG` — documented in
   `docs/OPERATOR_RUNBOOK.md` (new).
6. **Rustdoc pass**: crate `//!` overview + module guide in `lib.rs`, item docs for
   `conns`/`conns/streams`/`rns/config_builder` (comment-only).
7. Small read-only API additions: `PendingTransactionRegistry::in_flight_count` /
   `shielded_count`; `NodeServer::shutdown()`.

Baseline after: **997 passed / 37 ignored / 0 failed** (+9: 7 telemetry + 2 deploy
guard). Docker image builds green (`pneumatic:phase8`). Container smoke test
verified the boot sequence end-to-end (keystore create → tracing init → health
server on 0.0.0.0:9500 → RNS UDP 4242 up) and confirmed both binaries **fail
closed at boot without a reachable data service** — the composite's shielded-pool
load refuses to re-seed (corrupt-indistinguishable), the committer at the stake
snapshot. Runbook documents this; the full health→503→drain cycle under load
awaits a data-service deployment.
