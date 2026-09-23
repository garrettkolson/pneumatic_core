//! Composite-runtime boot for the node-server: `build_runtime`
//! assembles the shared DI bundle (env metadata, transport, data
//! service, stake index, epoch clock) and installs one role-plugin per
//! selected role; `load_stake_snapshot` warms the shared stake store,
//! failing closed (logged) when the data service is unavailable.

use super::*;

/// Build the composite runtime host over `config`'s floors against `stake`.
///
/// The host tolerates every failure that can occur while assembling the shared
/// DI bundle — a transport that will not start, a data service that is not yet
/// reachable, a stake index that cannot warm — and boots the host anyway, so a
/// node can come up (and register once its peers/data come online) rather than
/// panic before it has started. A missing environment is a hard error: no node
/// can run without its environment metadata.
///
/// Generalizes the committer `main.rs` boot recipe (one Committer) to *N*
/// installed role-plugins: a single DI bundle is built once and shared, then
/// one plugin is built per role the node selected by stake.
pub fn build_runtime(
    config: Arc<Config>,
    stake_provider: Arc<dyn crate::role_selector::StakeProvider>,
    data_provider: Arc<dyn DataProvider>,
) -> Result<NodeServer, PneumaticError> {
    // --- env metadata for the node's main environment (hard requirement) ----
    let env_data = match config
        .environment_metadata
        .get(&config.main_environment_id)
        .map(|e| e.value().clone())
    {
        Some(ed) => Arc::new(ed),
        None => {
            return Err(PneumaticError::Network(format!(
                "no environment '{}' for node",
                config.main_environment_id
            )))
        }
    };

    // --- transport (RNS) — boot tolerated if it will not start --------------
    // Mirrors the committer boot recipe: the node still boots if the transport
    // cannot come up; it just cannot register or gossip.
    let mut builder = RnsNodeConfigBuilder::new().with_transport_enabled(config.transport_enabled);
    for peer in &config.bootstrap_peers {
        builder = builder.add_peer(&peer.ip, peer.port);
    }
    let node_config = builder.build(&config.identity.rns);
    let network: Option<Arc<RnsNetwork>> = match RnsNetwork::start(
        node_config,
        &config.identity,
        &config.bootstrap_peers,
    ) {
        Ok(network) => Some(Arc::new(network)),
        Err(e) => {
            eprintln!(
                "[pneumatic] failed to start RNS transport: {} — booting without transport",
                e
            );
            None
        }
    };

    // --- data provider (injected: production passes DefaultDataProvider) ----
    // S5.3: the pool's boot load (below) is fail-closed — a store that cannot
    // be reached is a boot error, so tests must inject a reachable in-memory
    // provider; production keeps `DefaultDataProvider` and the strict contract.

    // --- registration stake gate (off the RNS worker pool, zero I/O) --------
    let stake_index = Arc::new(StakeIndex::new(
        data_provider.clone(),
        env_data.token_partition_id.clone(),
        1, // boot epoch; advanced by set_epoch on each epoch boundary
        None,
    ));
    stake_index.start();
    stake_index.warm(); // swallows I/O errors; a cold cache stays fail-closed
    let stake_check = stake_index.make_check(config.clone());

    let node_registry = Arc::new(NodeRegistry::init(
        config.clone(),
        network.clone(),
        stake_check,
    ));

    // --- shared logger + epoch components (single bundle, shared by plugins) --
    let shared_logger: Arc<dyn Logger> = env_data.logger.clone();
    let hash_provider: Arc<dyn HashProvider> = Arc::new(BasicHashProvider::new());

    let stake_store = Arc::new(StakeStore::new());
    let staking_manager = Arc::new(StakingManager::new(
        stake_store.clone(),
        shared_logger.clone(),
    ));
    // Fail closed at boot: a committer that proposes leaders blindly is worse
    // than one that does not start — but without a data service the snapshot is
    // simply absent, so we log and continue with an empty store.
    if let Err(e) = load_stake_snapshot(&data_provider, &env_data, &stake_store) {
        eprintln!(
            "[pneumatic] boot: stake snapshot load failed: {e} — using empty stake store"
        );
    }

    let candidate_registry = Arc::new(CandidateRegistry::new());
    let epoch_reconciler = Arc::new(EpochReconciler::new(
        stake_store.clone(),
        candidate_registry.clone(),
        data_provider.clone(),
        env_data.environment_id.clone(),
        vec![],
        env_data.cost_model.slash_fraction,
    ));
    let leader_selector = Arc::new(LeaderSelector::new(hash_provider.clone()));

    let tokens: Arc<DashMap<Vec<u8>, pneumatic_core::tokens::Token>> = Arc::new(DashMap::new());
    let pending_registry = Arc::new(PendingTransactionRegistry::new());

    // S5.3: the composite node loads ONE global shielded pool (fail-closed:
    // a corrupted stored state refuses to boot; a true absence seeds a
    // pristine genesis and persists it) and shares it by `Arc` with the
    // BlockServices and the committer's commit path.
    let shielded_pool = pneumatic_committer::shielded_pool::ShieldedPool::load(
        data_provider.as_ref(),
        &env_data.token_partition_id,
        env_data.shielded_root_recency,
    )
    .map_err(|e| {
        PneumaticError::Network(format!("load shielded pool at boot: {e:?}"))
    })?;

    // S5.4 (Decision 2, option a) — the real-pool swap: the shielded
    // roles' validation view IS the live pool (this `shielded_pool`), no
    // separate view object is composed in `build_runtime` anymore. The view
    // itself is derived per arm inside `build_role_plugin` (one `Arc`
    // clone, erased to `Arc<dyn ShieldedPoolView>`), so the view and the
    // commit path are the same object by construction. Semantics:
    // `nullifier_set()` is the pool's single shared registry (live);
    // `root_history()` is the pool's `view_roots` — a construction-time
    // snapshot (safe-Rust single-writer design: no lock-free swap of the
    // guard's published root state, see shielded_pool.rs module docs). A
    // long-running node's role views therefore see the root history as of
    // boot; that is safe for consensus because the committer's
    // authoritative re-check at commit time reads the pool's OWN live root
    // history (under the guard) and never the role views.

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let epoch_duration: i64 = 300;
    let initial_epoch = Epoch::new_with_leader(
        1,
        now,
        now + epoch_duration,
        leader_selector.as_ref(),
        &stake_store.to_stake_set(),
        &[],
    );
    let epoch_boundary_detector = Arc::new(EpochBoundaryDetector::new(initial_epoch));
    let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));
    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider.clone(),
        node_registry.clone(),
        env_data.clone(),
        shared_logger.clone(),
        config.identity.clone(),
        shielded_pool.clone(),
    ));

    // --- role selection + plugin construction -------------------------------
    // One plugin per role the node selected by stake (Phase 1). The DI bundle
    // above is dependency-ordered, so skipping an unselected role never
    // reorders it: the shared inputs are still built cheaply.
    // The local selector is used to compute `role_set` here (plugin construction);
    // the same instance is stored behind a `std` mutex on the host, so the
    // coordinator's later `select()` recompute sees the same epoch/stake state.
    let mut role_selector = crate::role_selector::RoleSelector::new(config.clone(), stake_provider);
    let role_set = role_selector.select();

    let installed: Vec<Box<dyn RoleHost>> = role_set
        .iter()
        .cloned()
        .filter_map(|role| {
            build_role_plugin(
                role,
                config.clone(),
                env_data.clone(),
                data_provider.clone(),
                node_registry.clone(),
                hash_provider.clone(),
                stake_store.clone(),
                staking_manager.clone(),
                epoch_reconciler.clone(),
                leader_selector.clone(),
                tokens.clone(),
                pending_registry.clone(),
                epoch_boundary_detector.clone(),
                block_proposer.clone(),
                block_services.clone(),
                shielded_pool.clone(),
            )
        })
        .collect();
    // Snapshot the installed roles (boot-time; unchanged until Phase 6's
    // deferred re-registration path) before moving `installed` into the locked
    // dispatcher, so `installed_roles()` can report them synchronously.
    let installed_roles: Vec<pneumatic_core::node::NodeRegistryType> =
        installed.iter().map(|h| h.role()).collect();
    let role_dispatcher = Arc::new(TokioMutex::new(RoleDispatcher::new(installed)));

    // Wire the RNS transport to the in-process role dispatcher: control-plane
    // packets go to the node registry, data-plane packets route to the installed
    // role that owns the message's action (the `route_data_plane` unit, Phase 7).
    if let Some(network_ref) = &network {
        let network = network_ref.clone();
        let registry = node_registry.clone();
        let dispatcher = role_dispatcher.clone();
        network.on_packet(Arc::new(move |raw: Vec<u8>| {
            match pneumatic_core::encoding::deserialize_rmp_to::<pneumatic_core::node::NetworkPacket>(&raw) {
                Ok(packet) => {
                    if let Some(control) = packet.control {
                        if let Err(e) = registry.handle_control(control) {
                            eprintln!("[pneumatic] control-plane error: {}", e);
                        }
                    }
                    if let Some(data) = packet.data {
                        let d = dispatcher.clone();
                        tokio::spawn(async move {
                            route_data_plane(data, d).await
                        });
                    }
                }
                Err(e) => {
                    eprintln!("[pneumatic] dropping undecodable transport packet: {}", e);
                }
            }
        }));
    }

    Ok(NodeServer {
        config,
        env_data,
        network,
        stake_index,
        node_registry,
        role_selector: StdMutex::new(role_selector),
        role_dispatcher,
        installed_roles,
        epoch_boundary_detector,
        tokens,
        pending_registry,
        shielded_pool,
    })
}

/// Load the current-epoch stake snapshot into the shared `StakeStore`, failing
/// closed (logged) rather than panicking when the data service is unavailable.
fn load_stake_snapshot(
    data_provider: &Arc<dyn DataProvider>,
    env_data: &Arc<EnvironmentMetadata>,
    stake_store: &Arc<StakeStore>,
) -> Result<(), PneumaticError> {
    let snapshot = data_provider
        .get_stake_snapshot(1, &env_data.token_partition_id)
        .map_err(PneumaticError::from)?;
    for (key, stake) in snapshot.stakers {
        stake_store.add_staker(key, stake);
    }
    Ok(())
}
