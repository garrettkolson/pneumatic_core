use std::env;
use std::sync::Arc;

use dashmap::DashMap;
use pneumatic_core::config::Config;
use pneumatic_core::crypto::BasicHashProvider;
use pneumatic_core::data::{DataProvider, DefaultDataProvider};
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::epoch::{BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::logging::Logger;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::stake_index::StakeIndex;
use pneumatic_core::node::{NetworkPacket, NodeRequest, NodeRegistryResponse, NodeRegistryType};
use pneumatic_core::node::NodeRequestType;
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::rns::config_builder::RnsNodeConfigBuilder;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::rns::wrapper::{AnnouncedIdentity, RnsNetwork};
use pneumatic_core::telemetry::{
    init_tracing, spawn_health_server, wait_for_shutdown_signal, HealthState, Metrics,
};

use pneumatic_committer::block_services::BlockServices;
use pneumatic_committer::committer::Committer;
use pneumatic_committer::shielded_pool::ShieldedPool;
use pneumatic_committer::epoch_manager::{
    EpochReconciler, LeaderSelector, StakeStore, StakingManager,
};

/// RNS listen IP: the configured node address, or all interfaces when the
/// config leaves it unspecified.
fn rns_listen_ip(config: &Config) -> String {
    if config.ip_address.is_unspecified() {
        "0.0.0.0".to_string()
    } else {
        config.ip_address.to_string()
    }
}

#[tokio::main]
async fn main() {
    // 1. Build config and environment metadata
    let config = match Config::build() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to build config: {:?}", e);
            return;
        }
    };

    // Get environment metadata for the main environment
    let env_id = &config.main_environment_id;
    let env_data = match config.environment_metadata.get(env_id) {
        Some(entry) => entry.value().clone(),
        None => {
            eprintln!("No environment metadata found for id: {}", env_id);
            return;
        }
    };

    // 1.5. Operator-facing telemetry (Phase 8). Structured tracing goes to
    // stdout (RUST_LOG-filtered, default info); the health/metrics HTTP
    // endpoint answers /health (200 → 503 when draining) and /metrics
    // (Prometheus text). Bind failure is logged and tolerated — health is an
    // ops affordance, not a consensus dependency (same tolerance as the RNS
    // boot path). PNEUMATIC_HEALTH_ADDR overrides the bind address
    // (e.g. "0.0.0.0:9500" in containers).
    init_tracing("committer");
    let health = std::sync::Arc::new(HealthState::new("committer"));
    let metrics = std::sync::Arc::new(Metrics::new());
    let health_addr: std::net::SocketAddr = env::var("PNEUMATIC_HEALTH_ADDR")
        .ok()
        .and_then(|a| a.parse().ok())
        .unwrap_or_else(|| "127.0.0.1:9500".parse().expect("static addr"));
    if let Err(e) = spawn_health_server(health_addr, health.clone(), metrics.clone()).await {
        tracing::warn!(error = %e, "health server unavailable; continuing without it");
    }

    // 2. Start the RNS transport. The node still boots if the transport can't
    //    come up (e.g. port conflict) — it just can't register or gossip.
    let mut builder = RnsNodeConfigBuilder::new()
        .with_listen_ip(rns_listen_ip(&config))
        .with_udp_port(config.rns_port)
        .with_transport_enabled(config.transport_enabled);
    for peer in &config.bootstrap_peers {
        builder = builder.add_peer(&peer.ip, peer.port);
    }
    let node_config = builder.build(&config.identity.rns);
    let network: Option<Arc<RnsNetwork>> =
        match RnsNetwork::start(node_config, &config.identity, &config.bootstrap_peers) {
            Ok(network) => Some(Arc::new(network)),
            Err(e) => {
                eprintln!(
                    "Failed to start RNS transport: {} — booting node without transport",
                    e
                );
                None
            }
        };

    // 3. Initialize NodeRegistry with a stake gate backed by the data service.
    //    Authenticate the data channel with the shared secret from the
    //    PNEUMATIC_DATA_SECRET env var when present; the framing + timeout
    //    hardening applies whether or not a secret is configured.
    //    PNEUMATIC_DATA_ADDR (host:port) points the channel at a remote data
    //    service — the container topology, where the default UDS-first local
    //    channel cannot reach a sibling container. An unparsable value warns
    //    and falls back to the local channel rather than refusing to boot.
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
    let mut data_provider = DefaultDataProvider::new();
    if let Some(addr) = data_addr {
        tracing::info!(%addr, "data service channel: remote TCP");
        data_provider = data_provider.with_source(pneumatic_core::conns::ConnTarget::Remote(addr));
    }
    let data_provider = match env::var("PNEUMATIC_DATA_SECRET") {
        Ok(secret) => Arc::new(data_provider.with_secret(secret.into_bytes())),
        Err(_) => {
            eprintln!(
                "PNEUMATIC_DATA_SECRET not set: the data service channel runs with the \
                 unauthenticated (legacy/test) framing. Set it in production."
            );
            Arc::new(data_provider)
        }
    };
    // 3.5. Build the registration stake gate OFF the RNS worker pool (AUDIT
    // Phase 4.4 / H7, H8). The closure below was replaced: it previously called
    // `data_provider.get_user`, a blocking framed TCP/UDS read, inside
    // `NodeRegistry::handle_register` — which the 4 plain std::thread RNS workers
    // invoke with no Tokio runtime. A hung data service could therefore hold one
    // worker for the full read timeout, and 4 concurrent registrations would
    // exhaust the pool and wedge the transport. StakeIndex keeps a background
    // std::thread that periodically loads the current-epoch stake snapshot into
    // an in-process pubkey->stake index, so the gate is a pure in-memory map
    // lookup with zero I/O: a hung data service can never touch the worker pool.
    // A cache miss (key absent) returns 0 stake ⇒ the gate rejects (fail
    // closed); a refresh error leaves the stale index in place ⇒ still fail
    // closed. `StakeCheck`'s signature is unchanged — only its backing closure
    // moved off the hot path.
    let stake_index = Arc::new(StakeIndex::new(
        data_provider.clone(),
        env_data.token_partition_id.clone(),
        1, // committer epoch at boot; advanced by set_epoch on each epoch boundary
        None,
    ));
    stake_index.start();
    // Synchronous warm-up before the gate is wired in, so a cold cache fails
    // registrations closed rather than open before the network starts.
    stake_index.warm();
    // make_check captures the config so the closure reads live floors without
    // a data-service round-trip. Pass an Arc so the returned StakeCheck owns its
    // config (mirrors the Arc<Config> handed to NodeRegistry::init below).
    let stake_check = stake_index.make_check(Arc::new(config.clone()));
    let node_registry = Arc::new(NodeRegistry::init(
        Arc::new(config.clone()),
        network.clone(),
        stake_check,
    ));

    // 4. Create Gossiper (Config is Clone — no second build)
    let gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Committer,
        config.clone(),
        60, // 60s TTL
        env_data.asym_crypto_provider.clone(),
    ));

    // 5. Bridge the transport to the control/data planes: control packets go
    //    to the node registry, data packets to the gossiper.
    if let Some(network_ref) = &network {
        let network = network_ref.clone();
        let send_net = network.clone();
        let registry = node_registry.clone();
        let gossip = gossiper.clone();
        network.on_packet(Arc::new(move |raw: Vec<u8>| {
            match deserialize_rmp_to::<NetworkPacket>(&raw) {
                Ok(packet) => {
                    if let Some(control) = packet.control {
                        if let Err(e) = registry.handle_control(control) {
                            eprintln!("[pneumatic] control-plane error: {}", e);
                        }
                    }
                    if let Some(data) = packet.data {
                        if let Ok(response) = deserialize_rmp_to::<NodeRegistryResponse>(&data) {
                            if let Err(e) = registry.handle_directory_response(&response) {
                                eprintln!("[pneumatic] directory response error: {}", e);
                            }
                        } else {
                            let _ = gossip.handle_message(data);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[pneumatic] dropping undecodable transport packet: {}", e);
                }
            }
        }));

        // 6. Discovery: when RNS announces a new peer, request its directory.
        let dir_cfg = config.clone();
        network.on_announce(Arc::new(move |announced: AnnouncedIdentity| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let rhash = announced.identity_hash.0;
            // A failed binding signature must surface as an error, not silently
            // degrade into an empty binding_signature that every peer rejects.
            let signature = NodeIdentity::sign_binding(
                &dir_cfg.identity,
                &rhash,
                &NodeRegistryType::Committer,
                &dir_cfg.node_registry_types,
            ).map_err(|e| format!("directory request sign_binding failed: {}", e))?;
            let payload = serialize_to_bytes_rmp(&NodeRequest {
                requester_key: dir_cfg.public_key.clone(),
                requester_rhash: dir_cfg.rhash,
                request_type: NodeRequestType::Request,
                requester_types: dir_cfg.node_registry_types.clone(),
                requested_type: NodeRegistryType::Committer,
                binding_signature: signature,
            })?;
            if payload.is_empty() {
                return Err("directory request serialized to an empty payload".into());
            }
            send_net
                .send_to(rhash, &payload)
                .map_err(|e| format!("directory request to {:02x?} failed: {}", rhash, e))?;
            Ok(())
        }));
    }

    // 4. Create shared env_data Arc (clone for shared ownership)
    let env_data = Arc::new(env_data);

    // 5. Create shared logger
    let shared_logger: Arc<dyn Logger> = env_data.logger.clone();

    // 6. Create StakeStore and StakingManager
    let stake_store = Arc::new(StakeStore::new());
    let staking_manager = Arc::new(StakingManager::new(
        stake_store.clone(),
        shared_logger.clone(),
    ));

    // Load the current epoch's stake set into the StakeStore so leader selection
    // runs against real stakes rather than an empty set. The committer persists
    // snapshots under token_partition_id, so read from the same partition. Fail
    // closed at boot: a committer that proposes leaders blindly is worse than one
    // that does not start.
    let snapshot = data_provider
        .get_stake_snapshot(1 /* current epoch */, &env_data.token_partition_id)
        .expect("load stake snapshot at boot");
    for (key, stake) in snapshot.stakers {
        stake_store.add_staker(key, stake);
    }

    // S5.3: load the global shielded pool (fail-closed at boot, like the
    // stake snapshot: a corrupted state refuses to start; a true absence
    // seeds a pristine genesis and persists it). Shared by Arc with the
    // BlockServices and the Committer's commit path.
    let shielded_pool = ShieldedPool::load(
        data_provider.as_ref(),
        &env_data.token_partition_id,
        env_data.shielded_root_recency,
    )
    .expect("load shielded pool at boot");

    // 7. Create EpochReconciler and LeaderSelector.
    // The CandidateRegistry is shared (cloned) between the reconciler and the
    // Committer so the reconciler's same-chain fork detection and the
    // Committer's commit-time conflict detection observe the same candidates.
    let candidate_registry = Arc::new(CandidateRegistry::new());
    let epoch_reconciler = Arc::new(EpochReconciler::new(
        stake_store.clone(),
        candidate_registry.clone(),
        data_provider.clone(),
        env_data.environment_id.clone(),
        vec![], // token IDs: populated dynamically via token distribution
        env_data.cost_model.slash_fraction,
    ));

    let hash_provider = Arc::new(BasicHashProvider::new());
    let leader_selector = Arc::new(LeaderSelector::new(hash_provider));

    // 8. Create Token cache
    let tokens = Arc::new(DashMap::new());

    // 9. Create PendingTransactionRegistry
    let pending_registry = Arc::new(PendingTransactionRegistry::new());
    // Metrics poller handle (cloned before the registry moves into the
    // Committer): read-only depth gauges for /metrics.
    let pending_for_metrics = pending_registry.clone();
    // Transport liveness gauge: the RNS network is built once at boot and
    // either exists or doesn't for the process lifetime.
    let transport_up = network.is_some() as u64;

    // 9.5. Create epoch tracking components
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let epoch_duration = 300; // 5 minutes
    let initial_epoch = Epoch::new_with_leader(
        1,
        now,
        now + epoch_duration,
        leader_selector.as_ref(),
        &stake_store.to_stake_set(),
        &[], // genesis: no prior block → empty prev_block_hash
    );
    let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
    let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));

    // 10. Create BlockServices
    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider.clone(),
        node_registry.clone(),
        env_data.clone(),
        shared_logger.clone(),
        config.identity.clone(),
        shielded_pool.clone(),
    ));

    // 11. Create Committer
    // Metrics poller handle (cloned before the registry moves into the
    // Committer): peer-count gauge for /metrics.
    let registry_for_metrics = node_registry.clone();
    let committer = Arc::new(Committer::new(
        env_data.clone(),
        config.public_key.clone(),
        config.identity.clone(),
        gossiper,
        block_services,
        node_registry,
        tokens,
        pending_registry,
        stake_store,
        staking_manager,
        epoch_reconciler,
        leader_selector,
        data_provider,
        0, // current_epoch_number
        Some(epoch_detector),
        block_proposer,
        epoch_duration,
        5000, // proposal_interval_ms: check every 5 seconds
        candidate_registry,
        shielded_pool,
    ));

    // 12. Wire up gossiper message handler
    let committer_clone = committer.clone();
    let logger_clone = shared_logger.clone();
    committer.initialize(move |message| {
        let committer = committer_clone.clone();
        let logger = logger_clone.clone();
        tokio::spawn(async move {
            if let Err(e) = committer.handle_message(message).await {
                logger.log(format!("Committer error: {:?}", e));
            }
        });
    });

    // 13. Start background epoch loop — polls for block proposals periodically,
    // stopping promptly when the shutdown channel flips (Phase 8: graceful
    // shutdown; the loop no longer runs to process exit).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let epoch_committer = committer.clone();
    // Advance the registration stake cache to the current epoch as it is
    // selected, so the off-thread gate consults the stake set frozen when each
    // epoch's leader was chosen (single source of truth: the committer's
    // current_epoch_number). The refresher's own std::thread continues polling
    // the data service on its own cadence for the target epoch.
    let epoch_stake_index = stake_index.clone();
    let mut epoch_shutdown = shutdown_rx.clone();
    let epoch_interval = committer.proposal_interval_ms();
    tokio::spawn(async move {
        loop {
            if let Err(e) = epoch_committer.run_epoch_loop().await {
                // Log but don't crash — epoch loop errors are non-fatal
                epoch_committer.logger()
                    .log(format!("Epoch loop error: {:?}", e));
            }
            epoch_stake_index.set_epoch(epoch_committer.current_epoch_number());
            // Sleep out the proposal interval, waking immediately if the
            // shutdown flag flips (a value change, not just a timer).
            tokio::select! {
                biased;
                changed = epoch_shutdown.changed() => {
                    if changed.is_err() || *epoch_shutdown.borrow_and_update() {
                        break;
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(epoch_interval)) => {}
            }
        }
        epoch_committer
            .logger()
            .log("Epoch loop stopped for shutdown".to_string());
    });

    // 13.5. Metrics poller (Phase 8): publishes cheap read-only depths and the
    // epoch number as gauges every 10s. Read-only — it holds no locks across
    // awaits and never mutates consensus state.
    let metrics_epoch = committer.clone();
    let metrics_registry = registry_for_metrics;
    let metrics_pending = pending_for_metrics;
    let mut metrics_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            metrics.set_gauge("pneumatic_epoch_current", metrics_epoch.current_epoch_number());
            metrics.set_gauge("pneumatic_transport_up", transport_up);
            let mut peers = 0u64;
            for node_type in [
                NodeRegistryType::Committer,
                NodeRegistryType::Sentinel,
                NodeRegistryType::Executor,
                NodeRegistryType::Finalizer,
                NodeRegistryType::Archiver,
            ] {
                if let Some(nodes) = metrics_registry.get_nodes(&node_type) {
                    peers += nodes.len() as u64;
                }
            }
            metrics.set_gauge("pneumatic_node_peers", peers);
            metrics.set_gauge(
                "pneumatic_pending_transactions",
                metrics_pending.in_flight_count() as u64,
            );
            metrics.set_gauge(
                "pneumatic_shielded_transactions",
                metrics_pending.shielded_count() as u64,
            );
            metrics.set_gauge("pneumatic_up", 1);
            if metrics_shutdown.changed().await.is_err() {
                break; // sender dropped: process is unwinding
            }
        }
    });

    // 14. Log startup and block on the shutdown signal (Phase 8: graceful
    // shutdown). Ordering matters:
    //   1. mark_stopping() → /health answers 503, so health-checking
    //      balancers/orchestrators stop sending work before anything drains;
    //   2. flip the watch channel → the epoch loop exits at its next check
    //      (immediately, not after the full proposal interval);
    //   3. stop the off-thread stake refresher;
    //   4. grace window for already-spawned message tasks, then exit —
    //      well inside docker stop's default 10s SIGKILL grace.
    shared_logger.log("Committer node started".to_string());
    tracing::info!(health_addr = %health_addr, "committer node started");

    wait_for_shutdown_signal().await;
    shared_logger.log("Shutdown signal received".to_string());
    tracing::info!("shutdown signal received; draining");
    health.mark_stopping();
    let _ = shutdown_tx.send(true);
    stake_index.stop();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    shared_logger.log("Committer node stopped cleanly".to_string());
    tracing::info!("committer node stopped cleanly");
}
