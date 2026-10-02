//! Composite-node binary entry point (Phase 8: real boot + graceful shutdown).
//!
//! Wires `Config::build()` into the composite runtime host
//! (`build_runtime` — the generalized committer boot recipe that installs one
//! role-plugin per role this node qualifies for by stake), serves the
//! operator health/metrics endpoint, drives the epoch-coordinator tick, and
//! blocks until Ctrl-C / SIGTERM. Shutdown ordering (mirrors the committer
//! binary):
//!
//! 1. mark health draining → `/health` 503, orchestrators stop routing;
//! 2. flip the watch channel → the coordinator loop exits without advancing;
//! 3. `NodeServer::shutdown()` fans shutdown to every installed role-plugin;
//! 4. short grace window for already-spawned work, then process exit —
//!    inside `docker stop`'s default 10s SIGKILL grace.
//!
//! Runtime configuration (env):
//! * `PNEUMATIC_DATA_SECRET` — HMAC secret for the data-service channel
//!   (same contract as the committer binary; unset = legacy unauthenticated
//!   framing, test/dev only).
//! * `PNEUMATIC_HEALTH_ADDR` — health/metrics bind address
//!   (default `127.0.0.1:9500`; use `0.0.0.0:9500` in containers).
//! * `PNEUMATIC_EPOCH_INTERVAL_MS` — coordinator tick cadence (default 5000).

use std::env;
use std::sync::Arc;

use pneumatic_core::config::Config;
use pneumatic_core::data::{DataProvider, DefaultDataProvider};
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::telemetry::{
    init_tracing, spawn_health_server, wait_for_shutdown_signal, HealthState, Metrics,
};

use pneumatic_node_server::node_server::build_runtime;
use pneumatic_node_server::role_selector::StakeProvider;

/// Production [`StakeProvider`]: answers own-stake queries from the data
/// service. Role selection runs at boot and on epoch boundaries only — never
/// on a message hot path — so a synchronous framed read here is acceptable;
/// any miss answers 0, so selection fails closed rather than installing a
/// role the node's stake cannot back.
struct DataStakeProvider {
    data: Arc<dyn DataProvider>,
    partition_id: String,
}

impl StakeProvider for DataStakeProvider {
    fn stake(&self, public_key: &[u8], _epoch: u64) -> u64 {
        self.data
            .get_user(&public_key.to_vec(), &self.partition_id)
            .map(|user| user.stake)
            .unwrap_or(0)
    }
}

/// Unix wall-clock seconds, as the epoch detector expects them.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::main]
async fn main() {
    // 1. Configuration (config.json + per-environment /env specs).
    let config = match Config::build() {
        Ok(cfg) => Arc::new(cfg),
        Err(e) => {
            eprintln!("Failed to build config: {:?}", e);
            return;
        }
    };
    init_tracing("node-server");
    let env_data = match config.environment_metadata.get(&config.main_environment_id) {
        Some(entry) => entry.value().clone(),
        None => {
            eprintln!(
                "No environment metadata found for id: {}",
                config.main_environment_id
            );
            return;
        }
    };

    // 2. Health/metrics endpoint (fail-soft: bind failure logs and continues).
    let health = Arc::new(HealthState::new("node-server"));
    let metrics = Arc::new(Metrics::new());
    let health_addr: std::net::SocketAddr = env::var("PNEUMATIC_HEALTH_ADDR")
        .ok()
        .and_then(|a| a.parse().ok())
        .unwrap_or_else(|| "127.0.0.1:9500".parse().expect("static addr"));
    if let Err(e) = spawn_health_server(health_addr, health.clone(), metrics.clone()).await {
        tracing::warn!(error = %e, "health server unavailable; continuing without it");
    }

    // 3. Data service channel (same secret + PNEUMATIC_DATA_ADDR contract as
    //    the committer binary: remote TCP when set, UDS-first local otherwise;
    //    an unparsable address warns and falls back to the local channel).
    let data_addr: Option<std::net::SocketAddr> = match env::var("PNEUMATIC_DATA_ADDR") {
        Ok(raw) => match raw.parse() {
            Ok(addr) => Some(addr),
            Err(e) => {
                tracing::warn!(
                    value = %raw, error = %e,
                    "PNEUMATIC_DATA_ADDR is not a valid host:port; using the local data channel"
                );
                None
            }
        },
        Err(_) => None,
    };
    let mut provider = DefaultDataProvider::new();
    if let Some(addr) = data_addr {
        tracing::info!(%addr, "data service channel: remote TCP");
        provider = provider.with_source(pneumatic_core::conns::ConnTarget::Remote(addr));
    }
    let data_provider: Arc<dyn DataProvider> = match env::var("PNEUMATIC_DATA_SECRET") {
        Ok(secret) => Arc::new(provider.with_secret(secret.into_bytes())),
        Err(_) => {
            eprintln!(
                "PNEUMATIC_DATA_SECRET not set: the data service channel runs with the \
                 unauthenticated (legacy/test) framing. Set it in production."
            );
            Arc::new(provider)
        }
    };

    // 4. Boot the composite runtime: shared DI bundle + one role-plugin per
    //    role this node's stake qualifies for. A missing environment is the
    //    only hard failure inside build_runtime (already checked above).
    let stake_provider = Arc::new(DataStakeProvider {
        data: data_provider.clone(),
        partition_id: env_data.token_partition_id.clone(),
    });
    let server = match build_runtime(config.clone(), stake_provider, data_provider.clone()) {
        Ok(server) => Arc::new(server),
        Err(e) => {
            tracing::error!(error = ?e, "composite runtime failed to boot");
            eprintln!("Composite runtime failed to boot: {:?}", e);
            return;
        }
    };
    tracing::info!(
        roles = ?server.installed_roles(),
        epoch = server.current_epoch(),
        health_addr = %health_addr,
        "node-server started"
    );

    // 5. Shutdown channel shared by the coordinator and the metrics poller.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // 6. Coordinator loop. The tick calls `poll_and_advance` (epoch expiry →
    // stake-gate refresh → advance fan-out → role-set recompute); a value
    // change on the watch channel stops it immediately rather than waiting
    // out the interval.
    let mut coord_shutdown = shutdown_rx.clone();
    let interval_ms: u64 = env::var("PNEUMATIC_EPOCH_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000);
    let coordinator = server.clone();
    tokio::spawn(async move {
        loop {
            coordinator.poll_and_advance(now_secs()).await;
            tokio::select! {
                biased;
                changed = coord_shutdown.changed() => {
                    if changed.is_err() || *coord_shutdown.borrow_and_update() {
                        break;
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(interval_ms)) => {}
            }
        }
        tracing::info!("epoch coordinator loop stopped for shutdown");
    });

    // 7. Metrics poller: read-only gauges from the host's shared state.
    let metrics_poller = server.clone();
    let mut poller_shutdown = shutdown_rx;
    tokio::spawn(async move {
        loop {
            metrics.set_gauge("pneumatic_epoch_current", metrics_poller.current_epoch());
            metrics.set_gauge(
                "pneumatic_installed_roles",
                metrics_poller.installed_roles().len() as u64,
            );
            let mut peers = 0u64;
            for node_type in [
                NodeRegistryType::Committer,
                NodeRegistryType::Sentinel,
                NodeRegistryType::Executor,
                NodeRegistryType::Finalizer,
                NodeRegistryType::Archiver,
            ] {
                if let Some(nodes) = metrics_poller.node_registry().get_nodes(&node_type) {
                    peers += nodes.len() as u64;
                }
            }
            metrics.set_gauge("pneumatic_node_peers", peers);
            metrics.set_gauge("pneumatic_tokens_cached", metrics_poller.tokens().len() as u64);
            metrics.set_gauge(
                "pneumatic_pending_transactions",
                metrics_poller.pending_registry().in_flight_count() as u64,
            );
            metrics.set_gauge(
                "pneumatic_shielded_transactions",
                metrics_poller.pending_registry().shielded_count() as u64,
            );
            metrics.set_gauge("pneumatic_up", 1);
            if poller_shutdown.changed().await.is_err() {
                break; // sender dropped: process is unwinding
            }
        }
    });

    // 8. Block on the shutdown signal, then drain in order (see module docs).
    wait_for_shutdown_signal().await;
    tracing::info!("shutdown signal received; draining");
    health.mark_stopping();
    let _ = shutdown_tx.send(true);
    server.shutdown().await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    tracing::info!("node-server stopped cleanly");
}
