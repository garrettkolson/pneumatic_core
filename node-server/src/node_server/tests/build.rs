//! Runtime-boot tests: plugin construction (sentinel arm pool view)
//! and the `build_runtime` boot discriminators (no-transport boot, stake
//! gate, epoch init, selected-roles-only install).
use super::helpers::*;
use super::super::*;

/// S5.4: the node-server DI bundle no longer plumbs a separate
/// `ShieldedPoolView` — the sentinel arm derives it from the SAME
/// `Arc<ShieldedPool>` that `block_services` and the committer arm hold
/// (the view and the commit path are one object by construction). This
/// test builds exactly that shared pool and asserts the arm builds a
/// genuine plugin (not `None`) that reports the sentinel role.
#[test]
fn build_role_plugin_sentinel_arm_builds_with_pool_view() {
    let config = runtime_config(
        vec![bad_peer()],
        type_config_select(NodeRegistryType::Sentinel),
    );
    // Same environment lookup as `build_runtime`: the main environment
    // the config references.
    let env_data = config
        .environment_metadata
        .get(&config.main_environment_id)
        .map(|e| Arc::new(e.value().clone()))
        .expect("test config carries its main environment");

    let data_provider: Arc<dyn DataProvider> = Arc::new(DefaultDataProvider::new());
    let hash_provider: Arc<dyn HashProvider> = Arc::new(BasicHashProvider::new());
    let shared_logger: Arc<dyn Logger> = env_data.logger.clone();

    let stake_index = Arc::new(StakeIndex::new(
        data_provider.clone(),
        env_data.token_partition_id.clone(),
        1,
        None,
    ));
    let stake_check = stake_index.make_check(config.clone());
    let node_registry = Arc::new(NodeRegistry::init(config.clone(), None, stake_check));

    let stake_store = Arc::new(StakeStore::new());
    let staking_manager = Arc::new(StakingManager::new(
        stake_store.clone(),
        shared_logger.clone(),
    ));
    let epoch_reconciler = Arc::new(EpochReconciler::new(
        stake_store.clone(),
        Arc::new(CandidateRegistry::new()),
        data_provider.clone(),
        env_data.environment_id.clone(),
        vec![],
        env_data.cost_model.slash_fraction,
    ));
    let leader_selector = Arc::new(LeaderSelector::new(hash_provider.clone()));

    let tokens: Arc<DashMap<Vec<u8>, pneumatic_core::tokens::Token>> =
        Arc::new(DashMap::new());
    let pending_registry = Arc::new(PendingTransactionRegistry::new());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let initial_epoch = Epoch::new_with_leader(
        1,
        now,
        now + 300,
        leader_selector.as_ref(),
        &stake_store.to_stake_set(),
        &[],
    );
    let epoch_boundary_detector = Arc::new(EpochBoundaryDetector::new(initial_epoch));
    let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));
    // S5.4: ONE pool, shared — `block_services` and the sentinel arm's
    // derived view are the same object, exactly as `build_runtime` wires
    // it now (the arm no longer takes a separate view parameter).
    let shielded_pool =
        Arc::new(pneumatic_committer::shielded_pool::ShieldedPool::new(10));
    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider.clone(),
        node_registry.clone(),
        env_data.clone(),
        shared_logger.clone(),
        config.identity.clone(),
        shielded_pool.clone(),
    ));

    let host = build_role_plugin(
        NodeRegistryType::Sentinel,
        config,
        env_data,
        data_provider,
        node_registry,
        hash_provider,
        stake_store,
        staking_manager,
        epoch_reconciler,
        leader_selector,
        tokens,
        pending_registry,
        epoch_boundary_detector,
        block_proposer,
        block_services,
        shielded_pool,
    )
    .expect("the sentinel arm builds a plugin");

    assert_eq!(
        host.role(),
        NodeRegistryType::Sentinel,
        "the constructed host reports the sentinel role"
    );
}

/// The host boots even when the RNS transport cannot start — a missing
/// or un-routable transport is tolerated, so a node can come up and later
/// register once its peers/data come online. This fails if `build_runtime`
/// hard-errors on a transport that will not start.
#[tokio::test]
async fn build_runtime_no_transport_booted_cleanly() {
    let cfg = runtime_config(vec![bad_peer()], type_config_floor(0));
    let provider = Arc::new(MapStakeProvider::with_default(2000));

    // No panic, no hard error — the host is constructible without transport.
    let server = build_runtime(cfg, provider, test_data_provider()).expect("host boots without transport");
    // The selected roles still install over the in-process path (registration
    // order: Committer, Sentinel, Executor, Finalizer). Archiver is selected
    // but has no plugin, so it is never installed.
    assert_eq!(
        server.installed_roles(),
        vec![
            NodeRegistryType::Committer,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Executor,
            NodeRegistryType::Finalizer,
        ],
        "the wired roles install without transport"
    );
    assert_eq!(
        server.selected_roles(),
        vec![
            NodeRegistryType::Committer,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Executor,
            NodeRegistryType::Finalizer,
            NodeRegistryType::Archiver,
        ]
    );
}

/// The stake gate drives installation: qualifying stake installs every
/// wired role; zero stake admits nothing and rejects every action.
/// Fails if the selection bundle ignores the stake floors (installs all).
#[tokio::test]
async fn build_runtime_wires_stake_gate() {
    // Qualifying (floor 0 + global 10): all four wired roles qualify.
    let qualifying = runtime_config(vec![bad_peer()], type_config_floor(0));
    let q = build_runtime(
        qualifying,
        Arc::new(MapStakeProvider::with_default(2000)),
        test_data_provider(),
    )
    .expect("host builds");
    assert_eq!(
        q.selected_roles(),
        vec![
            NodeRegistryType::Committer,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Executor,
            NodeRegistryType::Finalizer,
            NodeRegistryType::Archiver,
        ],
        "qualifying stake selects every wired role (Archiver selected, no plugin)"
    );
    assert_eq!(
        q.installed_roles(),
        vec![
            NodeRegistryType::Committer,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Executor,
            NodeRegistryType::Finalizer,
        ],
        "every wired role installs on qualifying stake"
    );

    // Zero stake: the gate admits nothing ⇒ no role installed, every action
    // rejected (fail closed). Fails if selection ignores zero-stake.
    let cold = runtime_config(vec![bad_peer()], type_config_floor(0));
    let c = build_runtime(
        cold,
        Arc::new(MapStakeProvider::with_default(0)),
        test_data_provider(),
    )
    .expect("host builds");
    assert!(
        c.installed_roles().is_empty(),
        "zero stake ⇒ no role installed, got {:?}",
        c.installed_roles()
    );
    assert!(matches!(c.dispatch(msg("Commit")).await, Err(RoleError::UnknownAction(_))));
}

/// The epoch bundle is initialized as part of `build_runtime`: building over
/// a single wired role constructs the epoch component without error, and the
/// Committer's inbound action reaches its handler (it errors on the empty
/// body as a downstream protocol error, never `UnknownAction` — proving the
/// handler was wired, not merely that the plugin exists).
#[tokio::test]
async fn build_runtime_initializes_epoch() {
    let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Committer));
    let provider = Arc::new(MapStakeProvider::with_default(2000));
    let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

    assert_eq!(server.installed_roles(), vec![NodeRegistryType::Committer]);
    let msg = msg("Commit");
    let outcome = server.dispatch(msg).await;
    // Reaches the Committer handler (Ok on success, Downstream on the
    // malformed body) — never UnknownAction, which would mean the handler
    // was not wired into the dispatcher.
    match outcome {
        Ok(()) => {}
        Err(RoleError::Downstream(_)) => {}
        other => panic!("Commit should reach the Committer handler, got {other:?}"),
    }
}

/// Only the role the node qualifies for is installed; a foreign action is
/// rejected rather than routed. Fails if the host installs every role
/// regardless of the per-type floors (selection ignored).
#[tokio::test]
async fn build_runtime_installs_only_selected_roles() {
    // Only the Committer qualifies (its floor is 0; the others are far
    // above any stake): exactly one role installs.
    let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Committer));
    let provider = Arc::new(MapStakeProvider::with_default(2000));
    let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

    assert_eq!(
        server.installed_roles(),
        vec![NodeRegistryType::Committer],
        "only the qualifying role installs"
    );
    // Executor's "Preload" is not installed ⇒ fail closed, never routed.
    assert!(matches!(
        server.dispatch(msg("Preload")).await,
        Err(RoleError::UnknownAction(_))
    ));
}
