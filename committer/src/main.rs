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

use pneumatic_committer::block_services::BlockServices;
use pneumatic_committer::committer::Committer;
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
    let data_provider = match env::var("PNEUMATIC_DATA_SECRET") {
        Ok(secret) => Arc::new(DefaultDataProvider::new().with_secret(secret.into_bytes())),
        Err(_) => {
            eprintln!(
                "PNEUMATIC_DATA_SECRET not set: the data service channel runs with the \
                 unauthenticated (legacy/test) framing. Set it in production."
            );
            Arc::new(DefaultDataProvider::new())
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
    ));

    // 11. Create Committer
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

    // 13. Start background epoch loop — polls for block proposals periodically
    let epoch_committer = committer.clone();
    // Advance the registration stake cache to the current epoch as it is
    // selected, so the off-thread gate consults the stake set frozen when each
    // epoch's leader was chosen (single source of truth: the committer's
    // current_epoch_number). The refresher's own std::thread continues polling
    // the data service on its own cadence for the target epoch.
    let epoch_stake_index = stake_index.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = epoch_committer.run_epoch_loop().await {
                // Log but don't crash — epoch loop errors are non-fatal
                epoch_committer.logger()
                    .log(format!("Epoch loop error: {:?}", e));
            }
            epoch_stake_index.set_epoch(epoch_committer.current_epoch_number());
            tokio::time::sleep(std::time::Duration::from_millis(
                epoch_committer.proposal_interval_ms(),
            ))
            .await;
        }
    });

    // 14. Log startup and block on shutdown
    shared_logger.log("Committer node started".to_string());

    // Block the main thread indefinitely (node runs until killed)
    // In production, this would listen for a shutdown signal
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
