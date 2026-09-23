//! In-process composite node-server runtime host.
//!
//! A single process that *is* the runtime; Committer, Sentinel, Executor, and
//! Finalizer are role-plugins it hosts (Phase 3). RNS stays the external wire.
//! The host builds one DI-ordered bundle shared by every installed plugin and
//! routes inbound messages to the single installed role that owns each action
//! via `RoleDispatcher` (Phase 2). This is a fresh in-process layer —
//! deliberately not the dead `pneumatic_core::server::ThreadPool` and not RNS.

use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as TokioMutex;

use dashmap::DashMap;
use ed25519_dalek::{SigningKey, VerifyingKey};

use pneumatic_core::config::Config;
use pneumatic_core::crypto::{BasicHashProvider, HashProvider};
use pneumatic_core::data::DataProvider;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::epoch::{BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::logging::Logger;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::stake_index::StakeIndex;
use pneumatic_core::registry::{PendingTransactionRegistry, TransactionSignatureRegistry};
use pneumatic_core::rns::config_builder::RnsNodeConfigBuilder;
use pneumatic_core::rns::wrapper::RnsNetwork;

use pneumatic_committer::epoch_manager::{EpochReconciler, StakeStore, StakingManager, LeaderSelector};
use pneumatic_committer::block_services::BlockServices;

use super::role_dispatcher::{RoleDispatcher, RoleError, RoleHandler, RoleHost};

/// `action` strings the Committer owns on the inbound bus.
const COMMITTER_ACTIONS: &'static [&'static str] = &["Commit"];
/// `action` strings the Executor owns (preload a transaction's data ahead of commit).
const EXECUTOR_ACTIONS: &'static [&'static str] = &["Preload"];
/// `action` strings the Sentinel owns (verify inbound transactions).
const SENTINEL_ACTIONS: &'static [&'static str] = &["Verify"];
/// `action` strings the Finalizer owns (sign / finalize blocks).
///
/// `"SignShielded"` / `"ShieldedVote"` are the shielded-transfer vote request
/// and vote (Phase S3.2, pneumatic-shielded-implementation-plan.md). They are
/// fail-closed by default: an action not listed here is rejected by
/// `RoleDispatcher::dispatch` (`UnknownAction`). Since S5.2 the arm bodies in
/// `handle` route to the real sign-the-public-outputs handlers.
const FINALIZER_ACTIONS: &'static [&'static str] =
    &["Sign", "Finalize", "SignShielded", "ShieldedVote"];

/// The composite runtime host. Owns the shared DI bundle plus three in-process
/// layers built across the composite plan: `RoleSelector` (Phase 1 — which roles
/// this node installs from its stake), `RoleDispatcher` (Phase 2/4 — routes an
/// inbound message to the one installed role that owns its action), and the
/// **epoch coordinator** (Phase 5 — polls the shared `EpochBoundaryDetector` and
/// fans each epoch advance to the installed plugins, and fans shutdown to them).
///
/// The installed role-plugins are held by the `RoleDispatcher` as `Box<dyn RoleHost>`
/// (Phase 5): a single boxed handle that is both a `RoleHandler` (inbound routing)
/// and a `RoleHost` (lifecycle), so the coordinator drives advance/shutdown through
/// the dispatcher rather than retaining a second parallel handle.
#[allow(dead_code)]
pub struct NodeServer {
    // Held in the composite for the later phases rather than read in Phase 3:
    // `config`/`env_data`/`network`/`node_registry` are the transport + registry
    // the lifecycle coordinator (Phase 5) re-registers and fans epoch advances
    // out over, and `stake_index`/`epoch_boundary_detector` are the off-thread
    // registration gate + epoch clock the coordinator polls. Read directly here
    // would be dead code now; they are the host's retained state for Phase 5+.
    config: Arc<Config>,
    env_data: Arc<EnvironmentMetadata>,
    network: Option<Arc<RnsNetwork>>,
    stake_index: Arc<StakeIndex>,
    node_registry: Arc<NodeRegistry>,
    // Phase 1 selector behind a `std` mutex: only the coordinator touches it
    // (`select` is `&mut`), never the hot path, so a plain sync mutex (no
    // await-across-guard) suffices.
    role_selector: StdMutex<super::role_selector::RoleSelector>,
    // Phase 2/4 dispatcher behind a `tokio` mutex: `dispatch` is held across an
    // await by the RNS bridge (Send-required), so the lock must be a
    // Send-across-await `tokio` mutex. `roll_forward`/`initiate_all_shutdown`
    // (the coordinator's lifecycle fan-outs, `&mut` on the boxed hosts) lock it
    // too. Wrapped in `Arc` so the RNS bridge closure (below) shares a handle to
    // the same dispatcher after it is constructed.
    role_dispatcher: Arc<TokioMutex<RoleDispatcher>>,
    // Snapshot of the roles this host installed at boot — read by `installed_roles`
    // synchronously without contending the `tokio` mutex (the tokio `Mutex` in the
    // pinned workspace has no sync `lock_blocking`). The set does not change until
    // Phase 6's deferred re-registration path, so a boot-time snapshot is faithful.
    installed_roles: Vec<pneumatic_core::node::NodeRegistryType>,
    // Lifecycle seed carried for Phase 5 (epoch coordinator).
    epoch_boundary_detector: Arc<EpochBoundaryDetector>,
    // S5.4: the composite's shared shielded state — the SAME `Arc`s the
    // role-plugins hold (committer commit path, block services, role views).
    // Retained here for the e2e tests' pool/chain lockstep assertions and
    // for the Phase 6 deferred re-registration path.
    tokens: Arc<DashMap<Vec<u8>, pneumatic_core::tokens::Token>>,
    pending_registry: Arc<PendingTransactionRegistry>,
    shielded_pool: Arc<pneumatic_committer::shielded_pool::ShieldedPool>,
}

impl NodeServer {
    /// The roles currently installed by this host, in registration order.
    pub fn installed_roles(&self) -> Vec<pneumatic_core::node::NodeRegistryType> {
        self.installed_roles.clone()
    }

    /// Route one inbound message to the single installed role that owns its
    /// action, fail-closed otherwise (mirrors `RoleDispatcher::dispatch`).
    ///
    /// The dispatcher lock is held across the handler await, so the method must
    /// be `Send` (the RNS bridge awaits it inside `tokio::spawn`); the
    /// `tokio` mutex (never `std`) is what makes the guard `Send`.
    pub async fn dispatch(&self, message: Message) -> Result<(), RoleError> {
        let guard = self.role_dispatcher.lock().await;
        guard.dispatch(message).await
    }

    /// The full qualifying role set this node selected by stake on the last
    /// `select()`, in `NodeRegistryType` order.
    pub fn selected_roles(&self) -> Vec<pneumatic_core::node::NodeRegistryType> {
        self.role_selector.lock().unwrap().selected_roles().to_vec()
    }

    /// The composite's global shielded pool (S5.4) — the same `Arc` the
    /// committer's commit path, the block services, and the sentinel/finalizer
    /// validation views hold. Exposed for the e2e tests' pool/chain lockstep
    /// assertions (never a second pool).
    pub fn shielded_pool(&self) -> Arc<pneumatic_committer::shielded_pool::ShieldedPool> {
        Arc::clone(&self.shielded_pool)
    }

    /// The composite's shared token cache — the chain state the committer
    /// commits into. Exposed for the e2e tests' chain-side lockstep
    /// assertions.
    pub fn tokens(&self) -> Arc<DashMap<Vec<u8>, pneumatic_core::tokens::Token>> {
        Arc::clone(&self.tokens)
    }

    /// The composite's shared pending-transaction registry (the parallel
    /// never-evicted shielded map included). Exposed for the e2e tests that
    /// seed the H12 validated-entry / shielded-payload pairs before driving
    /// a `Commit` through the dispatcher.
    pub fn pending_registry(&self) -> Arc<PendingTransactionRegistry> {
        Arc::clone(&self.pending_registry)
    }

    /// The composite's node registry (peer registrations, role lookups).
    /// Exposed for the e2e tests that register the node's own identity as a
    /// `Finalizer`/`Committer` peer with recording connections, exactly the
    /// way a split deployment registers its voting finalizer.
    pub fn node_registry(&self) -> Arc<NodeRegistry> {
        Arc::clone(&self.node_registry)
    }

    // --- Phase 5: epoch coordinator ----------------------------------------
    // The installed role-plugins live in the `RoleDispatcher` as `Box<dyn
    // RoleHost>`; the coordinator drives their lifecycle through the dispatcher.
    // Every lifecycle method is `&self` and locks internally, so the spawned
    // coordinator loop holds a single `Arc<Self>` and never takes a `&mut`
    // (the boxed `RoleHost` gives the Finalizer's `&mut self` its mutable handle
    // via the dispatcher's own `iter_mut` — no Mutex around the plugins).

    /// The epoch number the shared `EpochBoundaryDetector` currently holds — the
    /// epoch the Committer has last advanced it to (the Committer is the single
    /// writer; the coordinator only reads). Exposed so tests and the lifecycle
    /// coordinator can observe the current epoch.
    pub fn current_epoch(&self) -> u64 {
        self.epoch_boundary_detector.current_epoch.epoch_number
    }

    /// Fan one epoch advance to every installed role (Phase 5, `advance_epoch`
    /// fan-out on an epoch boundary). Delegates the fan-out to the dispatcher,
    /// which visits every installed host (the Committer's `advance_epoch` is a
    /// no-op that self-drives) and returns the roles visited.
    pub async fn roll_forward(
        &self,
        epoch: u64,
    ) -> Vec<pneumatic_core::node::NodeRegistryType> {
        let mut guard = self.role_dispatcher.lock().await;
        guard.roll_forward(epoch)
    }

    /// Recompute the role set this node qualifies for by stake, on an epoch
    /// boundary — the selection is re-evaluated because stake can change per
    /// epoch. Equivalent to a fresh `select()`; tracked by the coordinator so a
    /// changed set can trigger re-registration (Phase 6).
    pub fn recompute_role_set(&self) -> Vec<pneumatic_core::node::NodeRegistryType> {
        self.role_selector.lock().unwrap().select()
    }

    /// The coordinator's single epoch tick: when the shared detector reports the
    /// current epoch expired at `now`, advance — refresh the registration gate's
    /// off-thread index via `StakeIndex::set_epoch`, fan `advance_epoch` to the
    /// installed roles, and recompute the role set — and return `true`;
    /// otherwise return `false` (no advance this tick). No sleep; the spawned
    /// loop (`spawn_coordinator`) owns the cadence.
    pub async fn poll_and_advance(&self, now: i64) -> bool {
        if !self.epoch_boundary_detector.is_epoch_expired(now) {
            return false;
        }
        let epoch = self.current_epoch();
        self.stake_index.set_epoch(epoch);
        self.roll_forward(epoch).await;
        self.recompute_role_set();
        true
    }

    /// Fan graceful shutdown to every installed role (Committer + Finalizer run
    /// real shutdown; Sentinel + Executor no-op).
    pub async fn initiate_all_shutdown(&self) {
        let mut guard = self.role_dispatcher.lock().await;
        guard.initiate_all_shutdown().await;
    }

    /// Spawn the epoch-coordinator background loop: poll `poll_and_advance`
    /// every `interval_ms` until shutdown. The loop is thin glue (poll + sleep);
    /// `poll_and_advance` carries the substantive advance logic and is covered by
    /// the phase discriminators. The Committer's own `run_epoch_loop` (which
    /// advances the shared detector this loop watches) is spawned by the caller
    /// / Phase 7 end-to-end wiring.
    pub fn spawn_coordinator(self: Arc<Self>, interval_ms: u64) {
        tokio::spawn(async move {
            loop {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                self.poll_and_advance(now).await;
                tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
            }
        });
    }
}

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
    stake_provider: Arc<dyn super::role_selector::StakeProvider>,
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
    let mut role_selector = super::role_selector::RoleSelector::new(config.clone(), stake_provider);
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

/// Build the plugin for one selected role, or `None` for roles this host does
/// not install (`Archiver`, which has no plugin). The returned value is a boxed
/// `RoleHost` (a `RoleHandler` for inbound routing *and* a `RoleHost` for the
/// Phase-5 epoch-fan-out + shutdown) so the `RoleDispatcher` can route and
/// drive lifecycle through a single handle.
fn build_role_plugin(
    role: pneumatic_core::node::NodeRegistryType,
    config: Arc<Config>,
    env_data: Arc<EnvironmentMetadata>,
    data_provider: Arc<dyn DataProvider>,
    node_registry: Arc<NodeRegistry>,
    hash_provider: Arc<dyn HashProvider>,
    stake_store: Arc<StakeStore>,
    staking_manager: Arc<StakingManager>,
    epoch_reconciler: Arc<EpochReconciler>,
    leader_selector: Arc<LeaderSelector>,
    tokens: Arc<DashMap<Vec<u8>, pneumatic_core::tokens::Token>>,
    pending_registry: Arc<PendingTransactionRegistry>,
    epoch_boundary_detector: Arc<EpochBoundaryDetector>,
    block_proposer: Arc<BlockProposer>,
    block_services: Arc<BlockServices>,
    // S5.3: the global shielded pool (the committer arm's commit path).
    shielded_pool: Arc<pneumatic_committer::shielded_pool::ShieldedPool>,
) -> Option<Box<dyn RoleHost>> {
    use pneumatic_core::node::NodeRegistryType;
    // S5.4 (Decision 2, option a): the sentinel/finalizer arms consume the
    // SAME pool as their `ShieldedPoolView` — the role views read the
    // shared registry live; no separate view object is composed here (the
    // one that used to be built in `build_runtime` is gone).
    let shielded_pool_view: Arc<dyn pneumatic_core::shielded::ShieldedPoolView> =
        shielded_pool.clone() as Arc<dyn pneumatic_core::shielded::ShieldedPoolView>;
    match role {
        NodeRegistryType::Committer => {
            let gossiper = Arc::new(Gossiper::new(
                NodeRegistryType::Committer,
                config.as_ref().clone(),
                60,
                env_data.asym_crypto_provider.clone(),
            ));
            let committer = pneumatic_committer::Committer::new(
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
                0,
                Some((*epoch_boundary_detector).clone()),
                block_proposer,
                300,
                5000,
                Arc::new(CandidateRegistry::new()),
                shielded_pool,
            );
            Some(Box::new(committer))
        }
        NodeRegistryType::Executor => {
            let executor = pneumatic_executor::Executor::new(
                env_data.environment_id.clone(),
                config.public_key.clone(),
                config.identity.clone(),
                node_registry,
                data_provider,
                pending_registry,
                hash_provider,
                100,
            );
            Some(Box::new(executor))
        }
        NodeRegistryType::Sentinel => {
            let gossiper = Arc::new(Gossiper::new(
                NodeRegistryType::Sentinel,
                config.as_ref().clone(),
                60,
                env_data.asym_crypto_provider.clone(),
            ));
            let sentinel = pneumatic_sentinel::Sentinel::new(
                config.as_ref().clone(),
                env_data.clone(),
                node_registry,
                pending_registry,
                gossiper,
                data_provider,
                shielded_pool_view,
            );
            Some(Box::new(sentinel))
        }
        NodeRegistryType::Finalizer => {
            // Phase-3 construction: the finalizer's inbound is wired in Phase 4
            // (`initialize` is a stub), so the keys/quorum are the node's own
            // identity key + the bootstrap quorum. The handler forwards to that
            // stub.
            let signing_key = SigningKey::from_bytes(&[0u8; 32]);
            let verifying_key: VerifyingKey = signing_key.verifying_key();
            let signature_registry = Arc::new(TransactionSignatureRegistry::new());
            let finalizer = pneumatic_finalizer::Finalizer::new(
                env_data.environment_id.clone(),
                config.public_key.clone(),
                config.identity.clone(),
                node_registry,
                pending_registry,
                signature_registry,
                66.6,
                4,
                signing_key,
                verifying_key,
                hash_provider,
                vec![],
                0,
                vec![],
                1,
                data_provider,
                env_data.token_partition_id.clone(),
                // S5.2: the finalizer re-runs the shielded validation
                // against the same pool-view seam the sentinel arm consumes
                // (one view per environment — the S5.4 swap site).
                shielded_pool_view,
                env_data.clone(),
            );
            Some(Box::new(finalizer))
        }
        // Archiver has no role-plugin this host hosts.
        NodeRegistryType::Archiver => None,
    }
}

/// Route one RNS data-plane payload to the installed role that owns its action.
///
/// Phase 7: this is the extractable unit of the transport bridge. The RNS
/// `on_packet` handler (see `build_runtime`) parses each inbound frame into a
/// `NetworkPacket` and hands the data-plane bytes to this function; a reverted
/// bridge that drops the payload or never routes to the dispatcher fails the
/// `inbound_data_packet_routes_by_bridge` discriminator. Fail-closed: a payload
/// that does not parse as a `Message` (or whose action no installed role owns)
/// is surfaced, never silently swallowed.
async fn route_data_plane(
    data: Vec<u8>,
    dispatcher: Arc<TokioMutex<RoleDispatcher>>,
) -> Result<(), RoleError> {
    let message = pneumatic_core::encoding::deserialize_rmp_to::<pneumatic_core::messages::Message>(&data)
        .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("undecodable data-plane message: {e}"))))?;
    dispatcher.lock().await.dispatch(message).await
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

// ---------------------------------------------------------------------------
// RoleHandler impls — each installed role forwards its inbound bus actions to
// its real handler. Committer/Executor/Sentinel delegate to their real inbound
// method; the Finalizer routes `Sign` to `handle_signature` (audit C1) and
// fails closed on any other inbound action.
// ---------------------------------------------------------------------------

impl RoleHandler for pneumatic_committer::Committer {
    fn role(&self) -> pneumatic_core::node::NodeRegistryType {
        pneumatic_core::node::NodeRegistryType::Committer
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        COMMITTER_ACTIONS
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.handle_message(message)
                .await
                .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}"))))
        })
    }
}

impl RoleHandler for pneumatic_executor::Executor {
    fn role(&self) -> pneumatic_core::node::NodeRegistryType {
        pneumatic_core::node::NodeRegistryType::Executor
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        EXECUTOR_ACTIONS
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let tx_id = String::from_utf8_lossy(&message.body);
            self.preload_for_transaction(&tx_id)
                .await
                .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}"))))
        })
    }
}

impl RoleHandler for pneumatic_sentinel::Sentinel {
    fn role(&self) -> pneumatic_core::node::NodeRegistryType {
        pneumatic_core::node::NodeRegistryType::Sentinel
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        SENTINEL_ACTIONS
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.on_data_received(message.body)
                .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}"))))
        })
    }
}

impl RoleHandler for pneumatic_finalizer::Finalizer {
    fn role(&self) -> pneumatic_core::node::NodeRegistryType {
        pneumatic_core::node::NodeRegistryType::Finalizer
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        FINALIZER_ACTIONS
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match message.action.as_str() {
                // The voter inbound path: authenticate the executor's identity,
                // verify + accumulate its signature, or optimistic-finalize on
                // the first valid one. The real chokepoint for every voter
                // signature (audit C1).
                "Sign" => self
                    .handle_signature(&message)
                    .await
                    .map(|_| ())
                    .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}")))),
                // Shielded-transfer vote request from the Sentinel (S5.2).
                // Body is a `ShieldedTransaction`; the finalizer re-validates
                // against its own pool view, signs the stx hash, and fans the
                // vote out to the other finalizers.
                "SignShielded" => self
                    .handle_sign_shielded(&message)
                    .await
                    .map(|_| ())
                    .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}")))),
                // Shielded-transfer vote from a voting finalizer to the
                // collector Finalizer (S5.2). Body is a `TransactionSignature`;
                // quorum reached → assemble the shielded block and commit.
                "ShieldedVote" => self
                    .handle_shielded_vote(&message)
                    .await
                    .map(|_| ())
                    .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}")))),
                // Any other action this role owns is not a handled inbound
                // action; fail closed with a protocol-level error rather than
                // silently accepting it.
                other => Err(RoleError::Downstream(PneumaticError::Network(format!(
                    "finalizer: unhandled inbound action {other:?}"
                )))),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// RoleHost impls — Phase 5 lifecycle. `role()`/`allowed_actions()`/`handle`
// come from each role's `impl RoleHandler` above; these impls supply only the
// two lifecycle methods. Inherent calls use fully-qualified `Self::` so the
// compiler resolves them to the *inherent* method (which has the same name)
// rather than recursing into the trait method.
// ---------------------------------------------------------------------------

impl RoleHost for pneumatic_committer::Committer {
    // The Committer self-drives its epoch via its own `run_epoch_loop` (the
    // single writer of the epoch number), so the coordinator must not advance
    // it — the fan-out visits the Committer, whose `advance_epoch` is a no-op.
    fn advance_epoch(&mut self, _epoch: u64) {}

    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
    > {
        // Inherent `Committer::initiate_shutdown(&self)`, reached via
        // fully-qualified `Self::` so it does not recurse into this trait method.
        Box::pin(async move { Self::initiate_shutdown(self).await; })
    }
}

impl RoleHost for pneumatic_executor::Executor {
    // The Executor has no epoch advance and no shutdown lifecycle — both no-ops.
    fn advance_epoch(&mut self, _epoch: u64) {}

    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
    > {
        Box::pin(async move {})
    }
}

impl RoleHost for pneumatic_sentinel::Sentinel {
    // Sentinel advances from the external epoch signal: guards monotonicity and
    // invalidates its per-epoch caches.
    fn advance_epoch(&mut self, epoch: u64) {
        Self::advance_epoch(self, epoch);
    }

    // Sentinel has no shutdown lifecycle — a graceful no-op.
    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
    > {
        Box::pin(async move {})
    }
}

impl RoleHost for pneumatic_finalizer::Finalizer {
    // The Finalizer bumps its internal epoch counter + invalidates its stake
    // cache from the external signal. `advance_epoch(&mut self)`: the boxed
    // host gives this the mutable handle it needs (no Mutex).
    fn advance_epoch(&mut self, _epoch: u64) {
        Self::advance_epoch(self);
    }

    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
    > {
        Box::pin(async move { Self::initiate_shutdown(self).await; })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use dashmap::DashMap;
    use strum::IntoEnumIterator;

    use ed25519_dalek::{SigningKey, VerifyingKey};

    use pneumatic_core::blocks::{BlockFactory, FinalityStatus};
    use pneumatic_core::config::{BootstrapPeer, Config};
    use pneumatic_core::conns::Connection;
    use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
    use pneumatic_core::errors::PneumaticError;
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::messages::Message;
    use pneumatic_core::node::{NodeTypeConfig, NodeRegistryType};
    use pneumatic_core::rns::identity::NodeIdentity;
    use pneumatic_core::registry::TransactionSignatureRegistry;
    use pneumatic_core::tokens::Token;
    use pneumatic_core::transactions::{ShieldedTransaction, SignedTransaction, TransactionCommit};
    use pneumatic_core::user::User;
    use pneumatic_core::validation::ValidationSpecRegistry;

    // S5.4 fixtures (shared by the live composite tests).
    use pneumatic_core::data::{DataError, ShieldedPoolState};
    use pneumatic_core::epoch::StakeSet;

    use pneumatic_core::crypto::{BasicHashProvider, HashProvider};
    use pneumatic_core::data::{DataProvider, DefaultDataProvider};
    use pneumatic_core::epoch::{BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector};
    use pneumatic_core::logging::Logger;
    use pneumatic_core::node::registry::NodeRegistry;
    use pneumatic_core::node::stake_index::StakeIndex;
    use pneumatic_core::registry::PendingTransactionRegistry;
    use pneumatic_committer::block_services::BlockServices;
    use pneumatic_committer::epoch_manager::{
        EpochReconciler, LeaderSelector, StakeStore, StakingManager,
    };

    use crate::role_dispatcher::RoleError;
    use crate::role_selector::StakeProvider;
    use super::{
        build_runtime, build_role_plugin, route_data_plane, RoleDispatcher, RoleHandler,
        RoleHost,
    };

    /// In-memory `DataProvider` for runtime tests (S5.3): models a REACHABLE
    /// data service with no records — `get_shielded_pool` returns `Ok(None)`
    /// (the pool's boot load then seeds a pristine genesis) and every save is a
    /// no-op. All other reads answer "not found" in-process, so these tests
    /// never open a socket: production keeps `DefaultDataProvider` and its
    /// strict fail-closed boot contract.
    #[derive(Default)]
    struct MemoryDataProvider;

    impl DataProvider for MemoryDataProvider {
        fn get_token(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<pneumatic_core::tokens::Token, pneumatic_core::data::DataError> {
            Err(pneumatic_core::data::DataError::DataNotFound)
        }
        fn get_data(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Vec<u8>, pneumatic_core::data::DataError> {
            Err(pneumatic_core::data::DataError::DataNotFound)
        }
        fn get_user(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<pneumatic_core::user::User, pneumatic_core::data::DataError> {
            Err(pneumatic_core::data::DataError::DataNotFound)
        }
        fn get_stake_snapshot(&self, _epoch: u64, _partition_id: &str) -> Result<pneumatic_core::epoch::StakeSet, pneumatic_core::data::DataError> {
            Err(pneumatic_core::data::DataError::StoreNotFound)
        }
        fn save_stake_snapshot(&self, _epoch: u64, _snapshot: pneumatic_core::epoch::StakeSet, _partition_id: &str) -> Result<(), pneumatic_core::data::DataError> {
            Ok(())
        }
        fn get_executor_set(&self, _epoch: u64, _partition_id: &str) -> Result<pneumatic_core::epoch::ExecutorSet, pneumatic_core::data::DataError> {
            Err(pneumatic_core::data::DataError::StoreNotFound)
        }
        fn save_executor_set(&self, _epoch: u64, _set: pneumatic_core::epoch::ExecutorSet, _partition_id: &str) -> Result<(), pneumatic_core::data::DataError> {
            Ok(())
        }
        fn get_shielded_pool(&self, _partition_id: &str) -> Result<Option<pneumatic_core::data::ShieldedPoolState>, pneumatic_core::data::DataError> {
            Ok(None)
        }
        fn save_shielded_pool(&self, _state: &pneumatic_core::data::ShieldedPoolState, _partition_id: &str) -> Result<(), pneumatic_core::data::DataError> {
            Ok(())
        }
    }

    /// The data provider every `build_runtime` test injects.
    fn test_data_provider() -> Arc<dyn DataProvider> {
        Arc::new(MemoryDataProvider)
    }

    /// A complete, valid `EnvironmentMetadataSpec` — the canonical fixture used
    /// by the committer/sentinel integration tests, with `environment_id` set to
    /// `test_env` (matching `main_environment_id`). Every field the struct
    /// requires is present (its validate() also accepts it) so the spec both
    /// parses and loads into an `EnvironmentMetadata` with a populated
    /// `token_partition_id`, `asym_crypto_provider`, `cost_model`, and `logger`.
    const SPEC: &str = r#"{
        "environment_id": "test_env",
        "environment_name": "Test Environment",
        "partitions": [
            {"id": "token", "partition_type": "Token"},
            {"id": "reconciliation", "partition_type": "Slush"}
        ],
        "asym_crypto_provider": "Ed25519",
        "sym_crypto_provider": "AES-256-GCM",
        "serialization_provider": "rmp-serde",
        "quorum_percentage": 67.0,
        "override_quorum_percentage": 67.0,
        "max_risk": 1.0,
        "allowed_token_types": [],
        "trans_validation_specs": [],
        "block_validation_specs": [],
        "log_file": "/tmp/test.log",
        "shard_count": 1,
        "shard_quorum_percentage": 67.0
    }"#;

    /// The in-memory `StakeProvider` the selection path consults — fail-closed
    /// (0 on any miss), decoupled from the concrete `StakeIndex` map.
    struct MapStakeProvider {
        values: HashMap<u64, u64>,
        default: u64,
    }

    impl MapStakeProvider {
        fn with_default(default: u64) -> Self {
            Self { values: HashMap::new(), default }
        }
    }

    impl StakeProvider for MapStakeProvider {
        fn stake(&self, _public_key: &[u8], epoch: u64) -> u64 {
            self.values.get(&epoch).copied().unwrap_or(self.default)
        }
    }

    /// The node's environment registry, with `test_env` loaded from the spec.
    fn env_registry() -> Arc<DashMap<String, EnvironmentMetadata>> {
        let map = Arc::new(DashMap::new());
        let spec = serde_json::from_str::<EnvironmentMetadataSpec>(SPEC)
            .expect("valid test environment spec");
        let env = EnvironmentMetadata::load_from_spec(spec).expect("valid spec");
        map.insert(env.environment_id.clone(), env);
        map
    }

    /// Per-type floor: every role requires `floor` stake.
    fn type_config_floor(floor: u64) -> Arc<DashMap<NodeRegistryType, NodeTypeConfig>> {
        let cfgs = Arc::new(DashMap::new());
        for t in NodeRegistryType::iter() {
            cfgs.insert(t, NodeTypeConfig { min: 1, max: 1000, min_stake: floor });
        }
        cfgs
    }

    /// Per-type floors such that only `role` qualifies: that role's floor is 0,
    /// the others sit far above any stake. `select()` then yields exactly `role`.
    fn type_config_select(role: NodeRegistryType) -> Arc<DashMap<NodeRegistryType, NodeTypeConfig>> {
        let cfgs = Arc::new(DashMap::new());
        for t in NodeRegistryType::iter() {
            let min_stake = if t == role { 0 } else { 1_000_000 };
            cfgs.insert(t, NodeTypeConfig { min: 1, max: 1000, min_stake });
        }
        cfgs
    }

    /// A `Config` whose environment is `test_env`, `type_configs` is `configs`,
    /// and `bootstrap_peers` is `bootstrap` (a bad public key makes transport
    /// fail fast — keeps the host construction hermetic, no RNS binding).
    fn runtime_config(
        bootstrap: Vec<BootstrapPeer>,
        type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
    ) -> Arc<Config> {
        let mut cfg = Config::new_for_testing("test_env".into(), env_registry(), type_configs);
        cfg.bootstrap_peers = bootstrap;
        Arc::new(cfg)
    }

    /// A bad bootstrap public key (`hex::decode` fails in `RnsNetwork::start`)
    /// so the transport fails fast and the host boots without it — hermetic,
    /// no UDP binding across the shared workspace test runner.
    fn bad_peer() -> BootstrapPeer {
        BootstrapPeer {
            public_key: "not-a-valid-hex-key".to_string(),
            ip: "127.0.0.1".to_string(),
            port: 0,
        }
    }

    /// Canonical shape of a routing outcome, so two `Result<(), RoleError>`s
    /// (which do not derive `PartialEq`) can be compared for equality.
    fn outcome_tag(r: Result<(), RoleError>) -> &'static str {
        match r {
            Ok(()) => "ok",
            Err(RoleError::UnknownAction(_)) => "unknown_action",
            Err(RoleError::AmbiguousAction { .. }) => "ambiguous_action",
            Err(RoleError::Downstream(_)) => "downstream",
        }
    }

    fn msg(action: &str) -> Message {
        Message {
            chain_id: "env".into(),
            action: action.to_string(),
            body: vec![],
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        }
    }

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

    /// The RNS bridge routes an inbound data-plane message through the dispatcher
    /// to the installed role (Phase 7): a `NetworkPacket` whose data plane carries
    /// a "Commit" `Message` reaches the Committer handler (Downstream on the
    /// malformed body) — never `UnknownAction` — proving the bridge wired the
    /// dispatcher to the transport, not merely that the plugin exists. Reverting
    /// the bridge to drop the payload (never calling `route_data_plane`) makes
    /// this fail.
    #[tokio::test]
    async fn inbound_data_packet_routes_by_bridge() {
        let cfg =
            runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Committer));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

        let payload = serialize_to_bytes_rmp(&msg("Commit")).expect("payload serializes");

        // The bridge must route to the dispatcher the same way `dispatch` does.
        // Comparing the outcome shapes makes this a discriminator: a reverted
        // bridge that no-ops (returns `Ok` without dispatching) yields a result
        // that differs from the real handler path.
        let via_bridge = route_data_plane(payload, server.role_dispatcher.clone()).await;
        let via_direct = server.dispatch(msg("Commit")).await;
        assert_eq!(
            outcome_tag(via_bridge),
            outcome_tag(via_direct),
            "the transport bridge must route the Commit data-plane to the Committer handler exactly as dispatch does"
        );
    }

    /// The bridge reaches the dispatcher's routing logic (not a passthrough that
    /// accepts everything): a foreign action over the wired path surfaces
    /// `UnknownAction` from the dispatcher. Fails if the bridge swallows the
    /// payload without reaching the dispatcher's `dispatch`.
    #[tokio::test]
    async fn inbound_foreign_action_surfaces_through_bridge() {
        let cfg =
            runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Committer));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

        let payload = serialize_to_bytes_rmp(&msg("Confirm")).expect("payload serializes");

        assert!(matches!(
            route_data_plane(payload, server.role_dispatcher.clone()).await,
            Err(RoleError::UnknownAction(_))
        ));
    }

    /// The Finalizer's inbound handler is the real voter chokepoint, not a stub:
    /// a `Sign` reaching the host reaches `handle_signature`, which fails closed
    /// on the empty/invalid body with a protocol error — never the Phase-3 stub's
    /// `"finalizer inbound not yet wired"` error. Fails if the handler still
    /// returns the stub.
    #[tokio::test]
    async fn finalizer_inbound_handler_not_stub() {
        let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Finalizer));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

        assert_eq!(server.installed_roles(), vec![NodeRegistryType::Finalizer]);

        let outcome = server.dispatch(msg("Sign")).await;
        // Reaching the real handler means `handle_signature` ran on the empty
        // body and failed with a *different* protocol error (it errors on the
        // very first deserialize); a reverted stub would surface exactly
        // `"finalizer inbound not yet wired"`.
        let Err(RoleError::Downstream(PneumaticError::Network(msg))) = outcome else {
            panic!("finalizer 'Sign' must fail closed on the invalid body, got {outcome:?}");
        };
        assert_ne!(
            msg, "finalizer inbound not yet wired",
            "the finalizer inbound handler must be real, not the Phase-3 stub"
        );
    }

    /// The Executor's inbound action is routed through the dispatcher to the
    /// installed Executor (its `preload_for_transaction`), never routed to
    /// `UnknownAction`. Fails if the dispatcher does not forward `Preload` to
    /// the installed role.
    #[tokio::test]
    async fn executor_preload_routed_through_dispatcher() {
        let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Executor));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

        assert_eq!(server.installed_roles(), vec![NodeRegistryType::Executor]);

        let outcome = server.dispatch(msg("Preload")).await;
        match outcome {
            // Reaches the Executor's real handler — Ok on success, Downstream on
            // the empty body's protocol error — never UnknownAction.
            Ok(()) | Err(RoleError::Downstream(_)) => {}
            other => panic!("Preload should reach the Executor handler, got {other:?}"),
        }
    }

    /// The epoch coordinator's `roll_forward` fans `advance_epoch` to *every*
    /// installed plugin over the in-process dispatcher, and the coordinator
    /// returns the set of roles it visited. This fails if `NodeServer::roll_forward`
    /// drops the lock (empty result) or skips any installed role.
    #[tokio::test]
    async fn epoch_advance_fans_out_to_all_roles() {
        // Qualifying stake installs all four wired roles (Archiver selected but
        // has no plugin).
        let cfg = runtime_config(vec![bad_peer()], type_config_floor(0));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

        let advanced = server.roll_forward(2).await;
        // Every installed role was visited (registration order) …
        assert_eq!(advanced, server.installed_roles());
        // … and the fan-out reached the last installed role specifically (a
        // partial fan-out would miss it).
        assert!(advanced.contains(&NodeRegistryType::Finalizer));
    }

    /// `poll_and_advance` is the coordinator's gate: it is a no-op (returns
    /// `false`) while the epoch is live, and advances (returns `true`) once the
    /// shared `EpochBoundaryDetector` reports the epoch expired. Fails if the
    /// expiry gate is ignored (always-advance) or inverted (never-advance).
    #[tokio::test]
    async fn epoch_advance_poll_triggers_advance() {
        let cfg = runtime_config(vec![bad_peer()], type_config_floor(0));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

        // `now = 0` is long before the epoch's expiry (~build time + 300s) ⇒
        // the epoch is live, so poll must not advance.
        assert!(
            !server.poll_and_advance(0).await,
            "poll while the epoch is live must not advance"
        );

        // A `now` past the epoch's expiry (year 2126, far beyond build+300s)
        // ⇒ the epoch is expired, so poll must advance.
        let expired_now: i64 = 4_000_000_000;
        assert!(
            server.poll_and_advance(expired_now).await,
            "poll once the epoch is expired must advance"
        );
    }

    /// `recompute_role_set` re-evaluates role selection by stake — the recompute
    /// the epoch coordinator runs after advancing the registration gate. It must
    /// match `selected_roles()` (the boot-time selection). Fails if the recompute
    /// ignores stake (returns a fixed/empty set).
    #[tokio::test]
    async fn epoch_advance_recomputes_role_set() {
        // Qualifying stake selects every wired role + Archiver at boot.
        let cfg = runtime_config(vec![bad_peer()], type_config_floor(0));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

        let recomputed = server.recompute_role_set();
        assert_eq!(
            recomputed,
            server.selected_roles(),
            "recompute_role_set must re-evaluate stake to the same set selected at boot"
        );

        // With zero stake, re-evaluation admits nothing ⇒ the recompute is
        // stake-driven, not a fixed capture. Fails if recompute ignores stake.
        let cold_cfg = runtime_config(vec![bad_peer()], type_config_floor(0));
        let cold_server =
            build_runtime(cold_cfg, Arc::new(MapStakeProvider::with_default(0)), test_data_provider())
                .expect("host builds");
        assert!(
            cold_server.recompute_role_set().is_empty(),
            "recompute over zero stake must admit no roles"
        );
    }

    /// `initiate_all_shutdown` fans graceful shutdown to *every* installed role
    /// over the dispatcher. This integration test builds the real plugins and
    /// asserts the fan-out completes (does not panic / leave the dispatcher lock
    /// poisoned — a reverted fan-out that panics mid-loop would fail here). The
    /// per-plugin fan-out is proven rigorously at the dispatcher level by
    /// `initiate_all_shutdown_fans_to_all_hosts` (SpyHost records each shutdown).
    #[tokio::test]
    async fn shutdown_initiates_on_all_plugins() {
        // Qualifying stake installs all four wired roles.
        let cfg = runtime_config(vec![bad_peer()], type_config_floor(0));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

        let installed = server.installed_roles();
        // The fan-out runs (no panic) and does not mutate the install set —
        // shutdown is graceful, not a deregister.
        server.initiate_all_shutdown().await;
        assert_eq!(
            server.installed_roles(),
            installed,
            "shutdown must not remove any installed role"
        );
        // The dispatcher lock survived the fan-out — the host still routes.
        assert!(server.dispatch(msg("Commit")).await.is_err());
    }

    /// The `"SignShielded"` vote request reaches the installed Finalizer's real
    /// plugin handler (`handle_sign_shielded`) — not `UnknownAction` (action not
    /// registered in `FINALIZER_ACTIONS`) and not the else-fail-closed arm
    /// ("unhandled inbound action"). S5.2: the real handler's first gate is
    /// envelope authentication, so an unsigned message fails closed there with
    /// the distinct "envelope signature verification failed" crypto error,
    /// proving the message traversed dispatcher → plugin → match arm. Reverted
    /// (arm or `FINALIZER_ACTIONS` entry removed) this surfaces `UnknownAction`
    /// instead.
    #[tokio::test]
    async fn signshielded_request_reaches_finalizer_plugin_auth_gate() {
        let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Finalizer));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

        assert_eq!(server.installed_roles(), vec![NodeRegistryType::Finalizer]);

        // A wire-serializable `ShieldedTransaction` body — an empty body would
        // only prove the `Encoding` path; this proves the *arm* ran and reached
        // the auth gate before deserialization.
        let shielded_tx = ShieldedTransaction {
            id: "tx-shielded-1".to_string(),
            action: "ShieldedTransfer".to_string(),
            token_id: vec![1, 2, 3],
            spent_commitments: vec![[0u8; 32]],
            nullifiers: vec![[0u8; 32]],
            commitments: vec![[1u8; 32]],
            merkle_root: [2u8; 32],
            proof: vec![9, 9, 9],
            note_ciphertexts: vec![vec![7u8; 64]],
            fee: 0,
        };
        let body = serialize_to_bytes_rmp(&shielded_tx).expect("body serializes");
        let message = Message {
            chain_id: "env".to_string(),
            action: "SignShielded".to_string(),
            body,
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };

        match server.dispatch(message).await {
            // Routing reached the real handler, which fails closed at envelope
            // authentication. The dispatcher wraps the finalizer's `PneumaticError`
            // in `Network(«{e:?}»)` — the inner `Crypto` variant text must be
            // present, and no routing failure.
            Err(RoleError::Downstream(e)) => {
                let s = e.to_string();
                assert!(
                    s.contains("envelope signature verification failed"),
                    "expected the real handler's fail-closed auth gate error, got {s:?}"
                );
                assert!(
                    !s.contains("UnknownAction") && !s.contains("unhandled inbound action"),
                    "must route to the SignShielded arm, not reject at the dispatcher: {s:?}"
                );
            }
            other => panic!("expected the fail-closed Downstream auth error, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // S5.4 tests
    // -----------------------------------------------------------------------

    /// A `Connection` that records each sent payload verbatim (S5.4 e2e
    /// relay: the test re-dispatches what each role recorded on its peers).
    struct RecordingConnection {
        recorder: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait::async_trait]
    impl Connection for RecordingConnection {
        async fn send(&self, data: &Vec<u8>) -> Result<(), pneumatic_core::conns::ConnError> {
            self.recorder.lock().unwrap().push(data.clone());
            Ok(())
        }
    }

    /// In-memory `DataProvider` for the S5.4 live composite tests: the seeded
    /// pool state (the pool's boot load; saves update the copy), the opt-in
    /// self-verified token (the sentinel's token gates + the finalizer's
    /// `resolve_previous_hash`), the submitter user, and the epoch-1 stake
    /// snapshot (the finalizer's quorum math + the committer's conflict
    /// stakes all read from it — the same lazy snapshot path production uses).
    struct E2eDataProvider {
        pool_state: std::sync::Mutex<ShieldedPoolState>,
        token: Token,
        user_pk: Vec<u8>,
        stakers: std::collections::HashMap<Vec<u8>, u64>,
    }

    impl DataProvider for E2eDataProvider {
        fn get_token(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Token, DataError> {
            Ok(self.token.clone())
        }
        fn get_data(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Vec<u8>, DataError> {
            Err(DataError::DataNotFound)
        }
        fn get_user(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<User, DataError> {
            Ok(User {
                public_key: self.user_pk.clone(),
                fuel_balance: 10_000,
                stake: 0,
                nonce: 0,
            })
        }
        fn get_stake_snapshot(
            &self,
            _epoch: u64,
            _partition_id: &str,
        ) -> Result<StakeSet, DataError> {
            Ok(StakeSet { stakers: self.stakers.clone() })
        }
        fn save_stake_snapshot(
            &self,
            _epoch: u64,
            _snapshot: StakeSet,
            _partition_id: &str,
        ) -> Result<(), DataError> {
            Ok(())
        }
        fn get_executor_set(
            &self,
            _epoch: u64,
            _partition_id: &str,
        ) -> Result<pneumatic_core::epoch::ExecutorSet, DataError> {
            Err(DataError::StoreNotFound)
        }
        fn save_executor_set(
            &self,
            _epoch: u64,
            _set: pneumatic_core::epoch::ExecutorSet,
            _partition_id: &str,
        ) -> Result<(), DataError> {
            Ok(())
        }
        fn get_shielded_pool(
            &self,
            _partition_id: &str,
        ) -> Result<Option<ShieldedPoolState>, DataError> {
            Ok(Some(self.pool_state.lock().unwrap().clone()))
        }
        fn save_shielded_pool(
            &self,
            state: &ShieldedPoolState,
            _partition_id: &str,
        ) -> Result<(), DataError> {
            *self.pool_state.lock().unwrap() = state.clone();
            Ok(())
        }
    }

    /// A `Config` whose `test_env` carries the REAL shielded spec (defaults +
    /// `register_shielded`) in its transaction-spec registry: the composite
    /// sentinel and finalizer advisory gates look the spec up by name, and the
    /// minimal `SPEC` above leaves `trans_validation_specs` empty (that would
    /// fail closed `UnsupportedAction` before any transfer work). The
    /// committer's commit-time re-check is registry-independent (it builds
    /// `ShieldedValidationSpec::new()` directly), so only these two arms need
    /// the injection.
    fn runtime_config_with_shielded_spec(
        bootstrap: Vec<BootstrapPeer>,
        type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
    ) -> Arc<Config> {
        let registry = env_registry();
        {
            let mut ref_mut = registry.get_mut("test_env").expect("test env present");
            let env = ref_mut.value_mut();
            let mut specs = ValidationSpecRegistry::new();
            specs.register_defaults();
            specs.register_shielded();
            env.transaction_validation_specs = Arc::new(specs);
        }
        let mut cfg = Config::new_for_testing("test_env".into(), registry, type_configs);
        cfg.bootstrap_peers = bootstrap;
        Arc::new(cfg)
    }

    /// A test-build finalizer plugin: the REAL `Finalizer` (the composite
    /// finalizer arm's construction, over the shared live pool) whose
    /// `allowed_actions` reports `FINALIZER_ACTIONS` with `"SignShielded"`
    /// REMOVED. The `handle` body is a loud failure — the dispatcher must
    /// never reach it for the removed action.
    struct ReducedFinalizerPlugin(pneumatic_finalizer::Finalizer);

    impl RoleHandler for ReducedFinalizerPlugin {
        fn role(&self) -> NodeRegistryType {
            NodeRegistryType::Finalizer
        }
        fn allowed_actions(&self) -> &'static [&'static str] {
            &["Sign", "Finalize", "ShieldedVote"]
        }
        fn handle<'a>(
            &'a self,
            message: Message,
        ) -> std::pin::Pin<
            std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
        > {
            Box::pin(async move {
                Err(RoleError::Downstream(PneumaticError::Network(format!(
                    "reduced finalizer: handler called for {action:?} outside its reduced set",
                    action = message.action
                ))))
            })
        }
    }

    impl RoleHost for ReducedFinalizerPlugin {
        fn advance_epoch(&mut self, _epoch: u64) {
            pneumatic_finalizer::Finalizer::advance_epoch(&mut self.0);
        }
        fn initiate_shutdown<'a>(
            &'a mut self,
        ) -> std::pin::Pin<std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                pneumatic_finalizer::Finalizer::initiate_shutdown(&self.0).await
            })
        }
    }

    /// S5.4 discriminator: the `"SignShielded"` entry in `FINALIZER_ACTIONS`
    /// is the routing gate, not decoration. The real finalizer plugin (the
    /// same construction the composite's finalizer arm performs, over the
    /// shared live pool) is presented to a dispatcher under a reduced action
    /// set — `"SignShielded"` removed — and a `SignShielded` vote request
    /// must be rejected `UnknownAction` at the dispatcher, BEFORE any handler
    /// work. If the gate were not load-bearing (routing by role type rather
    /// than the reported action set), this test dispatches into the loud
    /// failure arm instead. The positive-side twin is
    /// `signshielded_request_reaches_finalizer_plugin_auth_gate` (entry
    /// present ⇒ routed to the real handler).
    #[tokio::test]
    async fn signshielded_removed_from_action_set_is_rejected_by_dispatcher() {
        let config = runtime_config(
            vec![bad_peer()],
            type_config_select(NodeRegistryType::Finalizer),
        );
        let env_data = config
            .environment_metadata
            .get(&config.main_environment_id)
            .map(|e| Arc::new(e.value().clone()))
            .expect("test config carries its main environment");

        let data_provider: Arc<dyn DataProvider> = Arc::new(DefaultDataProvider::new());
        let hash_provider: Arc<dyn HashProvider> = Arc::new(BasicHashProvider::new());
        let node_registry = Arc::new(NodeRegistry::init(
            config.clone(),
            None,
            StakeIndex::new(
                data_provider.clone(),
                env_data.token_partition_id.clone(),
                1,
                None,
            )
            .make_check(config.clone()),
        ));
        let pending_registry = Arc::new(PendingTransactionRegistry::new());

        // The composite finalizer arm's construction (S5.4: the view is the
        // shared live pool, erased to the seam — exactly the arm's in-fn
        // derivation).
        let signing_key = SigningKey::from_bytes(&[0u8; 32]);
        let verifying_key: VerifyingKey = signing_key.verifying_key();
        let signature_registry = Arc::new(TransactionSignatureRegistry::new());
        let shielded_pool =
            Arc::new(pneumatic_committer::shielded_pool::ShieldedPool::new(10));
        let shielded_pool_view: Arc<dyn pneumatic_core::shielded::ShieldedPoolView> =
            shielded_pool.clone() as Arc<dyn pneumatic_core::shielded::ShieldedPoolView>;
        let finalizer = pneumatic_finalizer::Finalizer::new(
            env_data.environment_id.clone(),
            config.public_key.clone(),
            config.identity.clone(),
            node_registry,
            pending_registry,
            signature_registry,
            66.6,
            4,
            signing_key,
            verifying_key,
            hash_provider,
            vec![],
            0,
            vec![],
            1,
            data_provider,
            env_data.token_partition_id.clone(),
            shielded_pool_view,
            env_data.clone(),
        );

        let plugin: Box<dyn RoleHost> = Box::new(ReducedFinalizerPlugin(finalizer));
        let dispatcher = RoleDispatcher::new(vec![plugin]);

        // The vote request (the same action the composite's sentinel emits).
        // An empty body is deliberately fine: the dispatcher inspects only
        // the action string, so this proves the gate fired pre-body.
        match dispatcher.dispatch(msg("SignShielded")).await {
            Err(RoleError::UnknownAction(action)) => assert_eq!(action, "SignShielded"),
            other => panic!(
                "expected the dispatcher's UnknownAction for the removed entry, got {other:?}"
            ),
        }
    }

    /// S5.4 composite e2e (LIVE — one halo2 prove): all four roles installed
    /// in ONE composite node, and a shielded transfer is driven through all
    /// four dispatch hops — Sentinel `Verify` → `SignShielded` → Finalizer
    /// `SignShielded` → `ShieldedVote` → Finalizer `ShieldedVote` → `Commit`
    /// → Committer `Commit` — with the inter-hop messages relayed by
    /// re-dispatching what each role recorded on its peers. The test drives
    /// ONLY the four relays: the sentinel does its own registration +
    /// finalizer assignment, the finalizer its own build + sign, and the
    /// committer materializes its pending entry from the authenticated wire
    /// block (H4). The terminal assertion is the pool/chain lockstep: the
    /// committer's commit path applied the delta to the SAME
    /// `Arc<ShieldedPool>` the roles' views read (S5.4's whole point — one
    /// pool, no separate view).
    #[tokio::test]
    #[ignore = "live halo2 prove (~1 min); run with --ignored"]
    async fn live_composite_shielded_e2e_all_four_hops_advance_the_pool() {
        use pneumatic_core::blocks::BlockFactory;
        use pneumatic_core::blocks::FinalityStatus;
        use pneumatic_core::shielded::{
            commit, root_to_bytes, IncrementalMerkleTree, ShieldedNote, DEFAULT_DEPTH,
        };
        use pneumatic_prover::{build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};
        use pasta_curves::pallas::Scalar as Fq;

        // --- the shielded universe: one input note, proven live ------------
        let input_note = ShieldedNote {
            value: 100,
            owner_pk: [1u8; 32],
            rho: Fq::from(1),
            rcm: Fq::from(2),
        };
        let spend_key = [0xABu8; 32];
        let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        let (root, _) = tree.append(&commit(&input_note));
        let pre_root = root_to_bytes(&root);
        let proof = tree.membership_proof(0);

        // Seed the pool's persisted state with the input note's leaf, so the
        // boot-loaded pool's root history contains the root this tx proves
        // against (within recency) — the same seeding the committer's live
        // pool tests use. `from_state` rebuilds the tree from the delta's
        // leaves, so the seed delta must carry the leaf it produced.
        let leaf =
            root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&input_note)));
        let seed_state = ShieldedPoolState {
            root: pre_root,
            leaf_count: 1,
            leaves: vec![leaf],
            nullifiers: vec![],
            applied: vec![e2e_seed_delta(vec![0xAA; 32], vec![leaf], pre_root)],
        };

        // --- the composite's own identity is the voting finalizer ----------
        // (a split deployment registers its voting finalizer the same way).
        // The key must sit in BOTH the Finalizer bucket (the finalizer's C1
        // gate on SignShielded/ShieldedVote, and the committer's Commit role
        // gate — `Commit` requires the signer's role set to include
        // Finalizer) and the Committer bucket (so the finalizer's outbound
        // Commit has a recorded destination to relay from).
        let cfg = runtime_config_with_shielded_spec(vec![bad_peer()], type_config_floor(0));
        let own_key = cfg.public_key.clone();

        // The token must exist in BOTH the composite's shared token cache
        // (the committer's commit path) and the data provider (the
        // finalizer's `resolve_previous_hash` reads the PROVIDER, not the
        // cache) — the SAME genesis-chained token in both, built BEFORE the
        // provider, so the finalizer's prev-hash and the committer's strict
        // linkage check agree.
        let mut token = make_e2e_token();
        {
            let mut genesis = pneumatic_core::blocks::Block {
                signed_trans: SignedTransaction::test_transaction(),
                token_metadata: std::collections::HashMap::new(),
                previous_hash: vec![42u8; 32],
                timestamp: 0,
                current_hash: vec![],
                finality_status: FinalityStatus::Optimistic,
                proposer_key: vec![],
                epoch_number: 0,
            };
            genesis.current_hash =
                BlockFactory::create_hash(&genesis).expect("genesis hashes");
            token.blockchain.add_block(genesis);
        }

        // One in-memory data provider: the seeded pool state, the opt-in
        // self-verified token, the user, and an epoch-1 stake snapshot in
        // which this node's own key is the sole (hence 100%) staker.
        let mut stakers = std::collections::HashMap::new();
        stakers.insert(own_key.clone(), 100u64);
        let dp = Arc::new(E2eDataProvider {
            pool_state: std::sync::Mutex::new(seed_state),
            token: token.clone(),
            user_pk: b"alice".to_vec(),
            stakers,
        });

        let server = build_runtime(cfg, Arc::new(MapStakeProvider::with_default(2000)), dp.clone())
            .expect("composite boots with the seeded pool");
        let pool = server.shielded_pool();
        assert_eq!(pool.leaf_count(), 1, "boot load rebuilt the seeded pool");
        assert_eq!(pool.applied_count(), 1, "the seed delta survived the boot load");

        // Register the composite's own identity as Finalizer + Committer
        // peers with recording connections, so every outbound hop is
        // captured for the relay.
        let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
        assert!(
            server.node_registry().register_peer(
                own_key.clone(), [9u8; 16], &NodeRegistryType::Finalizer,
                Box::new(RecordingConnection { recorder: recorder.clone() }),
            ),
            "own finalizer peer registers"
        );
        assert!(
            server.node_registry().register_peer(
                own_key.clone(), [9u8; 16], &NodeRegistryType::Committer,
                Box::new(RecordingConnection { recorder: recorder.clone() }),
            ),
            "own committer peer registers"
        );

        // Prove the live transfer against the pool's seeded root: 100 = 90 + 10.
        let recipient = ShieldedIdentity {
            spend: SpendKey::from_seed([2u8; 32]),
            identity: pneumatic_core::crypto::Ed25519Provider::generate(),
        };
        let (stx, _) = build_shielded_tx(
            &[1u8], &[input_note], &[&spend_key], &[proof],
            pre_root, &[NoteOutput::new(90, recipient)], 10,
        )
        .expect("the real prove must succeed");
        let nullifier = stx.nullifiers[0];

        // Install the SAME genesis-chained token in the composite's shared
        // token cache (the committer's commit path).
        {
            let tokens = server.tokens();
            tokens.entry(vec![1]).or_insert(token);
        }
        let tokens_before = server
            .tokens()
            .get(&vec![1])
            .map(|t| t.blockchain.get_count())
            .unwrap_or(0);

        // --- hop 1: the sentinel's `Verify` (inner ShieldedTransfer) -------
        // The sentinel performs its own advisory gate, token gates, the
        // parallel `register_shielded`, and the deterministic finalizer
        // assignment, and records the outbound SignShielded — nothing else
        // to pre-stage (the committer materializes its pending entry from
        // the authenticated wire block, H4).
        let verify_msg = Message {
            chain_id: "env".to_string(),
            action: "Verify".to_string(),
            body: serialize_to_bytes_rmp(
                &Message {
                    chain_id: "env".to_string(),
                    action: "ShieldedTransfer".to_string(),
                    body: serialize_to_bytes_rmp(&stx).expect("stx serializes"),
                    signature: vec![],
                    public_key: vec![],
                    stake_set: None,
                },
            )
            .expect("inner message serializes"),
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };
        server
            .dispatch(verify_msg)
            .await
            .expect("hop 1: the sentinel accepts the live transfer");

        // Hop 2: relay the sentinel's recorded SignShielded to the finalizer.
        let sign = next_recorded(&recorder, "SignShielded").await;
        server.dispatch(sign).await.expect("hop 2: the finalizer signs the vote");

        // Hop 3: relay the finalizer's recorded ShieldedVote.
        let vote = next_recorded(&recorder, "ShieldedVote").await;
        server.dispatch(vote).await.expect("hop 3: quorum finalizes the transfer");

        // Hop 4: relay the finalizer's recorded Commit to the committer.
        let commit_msg = next_recorded(&recorder, "Commit").await;
        server
            .dispatch(commit_msg.clone())
            .await
            .expect("hop 4: the committer commits the shielded block");
        let commit: TransactionCommit =
            deserialize_rmp_to(&commit_msg.body).expect("commit body deserializes");

        // --- the lockstep assertion: ONE pool advanced, the chain followed -
        assert_eq!(pool.leaf_count(), 2, "the output commitment leaf was appended");
        assert_eq!(pool.applied_count(), 2, "seed delta + this commit's delta");
        assert!(pool.nullifiers().contains(nullifier), "the spend is recorded");
        assert_eq!(
            pool.current_root(),
            pool_tree_root(&pool),
            "the root-history tip equals the rebuilt tree root"
        );
        let tip = server
            .tokens()
            .get(&vec![1])
            .unwrap()
            .blockchain
            .get_current_chain_state()
            .last_hash_in;
        assert_eq!(
            tip,
            commit.proposed_block.current_hash,
            "the committed block is the chain tip"
        );
        let count = server.tokens().get(&vec![1]).unwrap().blockchain.get_count();
        assert_eq!(count, tokens_before + 1, "chain advanced exactly one block");
    }

    /// S5.4 conflict check (LIVE — two halo2 proves): a shielded block that
    /// LOSES its tip conflict is rolled back lockstep — the loser's pool
    /// delta is reverted (leaves, nullifiers, root history) through the
    /// COMPOSITE's dispatch path (dispatcher → committer plugin →
    /// `commit_block` rollback branch → pool `revert_update`), proving the
    /// machinery needs no epoch-logic changes: the lockstep the committer
    /// owns works unchanged inside the composite. The winner's delta stays
    /// applied. Finalizer-free by design: both Commits are hand-signed by
    /// two registered finalizer identities with UNEQUAL stakes (100 vs 200 —
    /// an equal-stake tie would fail closed) — the committer's conflict
    /// resolution is the unit under test, not the finalizer's quorum math.
    #[tokio::test]
    #[ignore = "live halo2 prove x2 (~2 min); run with --ignored"]
    async fn live_composite_conflict_rollback_lockstep() {
        use pneumatic_core::blocks::BlockFactory;
        use pneumatic_core::blocks::FinalityStatus;
        use pneumatic_core::crypto::AsymCryptoProvider;
        use pneumatic_core::shielded::{
            commit, root_to_bytes, IncrementalMerkleTree, ShieldedNote, DEFAULT_DEPTH,
        };
        use pneumatic_prover::{build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};
        use pasta_curves::pallas::Scalar as Fq;

        // TWO input notes in ONE tree: the sibling proposals A and B each
        // prove against the FINAL root (the pool's current root — the root
        // history both recency checks run against).
        let note_a = ShieldedNote {
            value: 100,
            owner_pk: [1u8; 32],
            rho: Fq::from(1),
            rcm: Fq::from(2),
        };
        let note_b = ShieldedNote {
            value: 200,
            owner_pk: [2u8; 32],
            rho: Fq::from(3),
            rcm: Fq::from(4),
        };
        let spend_a = [0xA1u8; 32];
        let spend_b = [0xB2u8; 32];
        let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        let (root_a, _) = tree.append(&commit(&note_a));
        let (root_b, proof_b) = tree.append(&commit(&note_b));
        let root_a = root_to_bytes(&root_a);
        let root_b = root_to_bytes(&root_b);
        let leaf_a = root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&note_a)));
        let leaf_b = root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&note_b)));
        // note_a's membership re-derived against the FINAL root.
        let proof_a = tree.membership_proof(0);

        // Seed the pool with BOTH leaves in two prior deltas (the final root
        // as the tip — the pool's boot state).
        let seed_state = ShieldedPoolState {
            root: root_b,
            leaf_count: 2,
            leaves: vec![leaf_a, leaf_b],
            nullifiers: vec![],
            applied: vec![
                e2e_seed_delta(vec![0xAA; 32], vec![leaf_a], root_a),
                e2e_seed_delta(vec![0xAB; 32], vec![leaf_b], root_b),
            ],
        };

        // Two distinct finalizer identities: the conflicting proposers.
        let identity_a = NodeIdentity::generate_in_memory();
        let identity_b = NodeIdentity::generate_in_memory();
        let key_a = identity_a.ed25519.public_key().expect("A pubkey");
        let key_b = identity_b.ed25519.public_key().expect("B pubkey");

        // Committer-only composite: the unit under test is the committer's
        // commit path (the S5.4 pool swap) — no sentinel/finalizer arms.
        let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Committer));
        let mut stakers = std::collections::HashMap::new();
        stakers.insert(key_a.clone(), 100u64);
        stakers.insert(key_b.clone(), 200u64);
        let dp = Arc::new(E2eDataProvider {
            pool_state: std::sync::Mutex::new(seed_state),
            token: make_e2e_token(),
            user_pk: b"alice".to_vec(),
            stakers,
        });
        let server = build_runtime(cfg, Arc::new(MapStakeProvider::with_default(2000)), dp)
            .expect("composite boots with the seeded pool");
        let pool = server.shielded_pool();
        assert_eq!(pool.leaf_count(), 2, "boot load rebuilt the seeded pool");
        assert_eq!(pool.applied_count(), 2, "both seed deltas survived the boot load");

        // Prove BOTH sibling spends live, against the shared FINAL root:
        // A: 100 = 90 + 10; B: 200 = 190 + 10.
        let recipient_a = ShieldedIdentity {
            spend: SpendKey::from_seed([2u8; 32]),
            identity: pneumatic_core::crypto::Ed25519Provider::generate(),
        };
        let recipient_b = ShieldedIdentity {
            spend: SpendKey::from_seed([3u8; 32]),
            identity: pneumatic_core::crypto::Ed25519Provider::generate(),
        };
        let (stx_a, _) = build_shielded_tx(
            &[1u8], &[note_a], &[&spend_a], &[proof_a],
            root_b, &[NoteOutput::new(90, recipient_a)], 10,
        )
        .expect("prove A");
        let (stx_b, _) = build_shielded_tx(
            &[1u8], &[note_b], &[&spend_b], &[proof_b],
            root_b, &[NoteOutput::new(190, recipient_b)], 10,
        )
        .expect("prove B");
        let null_a = stx_a.nullifiers[0];
        let null_b = stx_b.nullifiers[0];

        // Bootstrap the token (shared cache) with a genesis.
        {
            let tokens = server.tokens();
            let mut t = tokens.entry(vec![1]).or_insert(make_e2e_token());
            let mut genesis = pneumatic_core::blocks::Block {
                signed_trans: SignedTransaction::test_transaction(),
                token_metadata: std::collections::HashMap::new(),
                previous_hash: vec![42u8; 32],
                timestamp: 0,
                current_hash: vec![],
                finality_status: FinalityStatus::Optimistic,
                proposer_key: vec![],
                epoch_number: 0,
            };
            genesis.current_hash = BlockFactory::create_hash(&genesis).expect("genesis hashes");
            t.blockchain.add_block(genesis);
        }

        // Register BOTH proposers as Finalizer peers (recording connections
        // — nothing is actually sent), so the committer's Commit auth
        // resolves their role sets and their stake from the snapshot.
        let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
        for (key, rhash) in
            [(key_a.clone(), [0xA0u8; 16]), (key_b.clone(), [0xB0u8; 16])]
        {
            assert!(
                server.node_registry().register_peer(
                    key, rhash, &NodeRegistryType::Finalizer,
                    Box::new(RecordingConnection { recorder: recorder.clone() }),
                ),
                "proposer peer registers"
            );
        }

        // H12 shielded pairing for both siblings: each wire block's stx must
        // hash-match a registered shielded entry (no plain entries — the
        // committer materializes them from the wire block, H4).
        let registry = server.pending_registry();
        registry.register_shielded(&stx_a).expect("A's payload registers");
        registry.register_shielded(&stx_b).expect("B's payload registers");

        // A commits first: the block is chained off the tip captured NOW
        // (the genesis hash) and signed by proposer A's registered
        // finalizer identity.
        let genesis_tip = server
            .tokens()
            .get(&vec![1])
            .map(|t| {
                let s = t.blockchain.get_current_chain_state();
                if s.last_hash_in.is_empty() {
                    Vec::new()
                } else {
                    s.last_hash_in
                }
            })
            .unwrap_or_default();
        let block_a = make_e2e_shielded_block(&stx_a, genesis_tip.clone());
        let commit_a = e2e_commit(&stx_a, &block_a);
        server
            .dispatch(e2e_commit_message(&commit_a, &identity_a))
            .await
            .expect("A commits and becomes the tip");
        assert!(pool.nullifiers().contains(null_a), "A's delta is applied");

        // B (higher stake) commits at the SAME chain position — chained off
        // the genesis tip captured BEFORE A committed, so the conflict is on
        // the position, not the prev hash. B wins; A is rolled back
        // lockstep through the composite's commit path.
        let block_b = make_e2e_shielded_block(&stx_b, genesis_tip);
        let commit_b = e2e_commit(&stx_b, &block_b);
        server
            .dispatch(e2e_commit_message(&commit_b, &identity_b))
            .await
            .expect("B wins the conflict and commits");

        // Chain: A is gone, B is the sole block at the position.
        let tip = server
            .tokens()
            .get(&vec![1])
            .unwrap()
            .blockchain
            .get_current_chain_state()
            .last_hash_in;
        assert_eq!(tip, commit_b.proposed_block.current_hash, "B is the tip");
        assert_eq!(
            server.tokens().get(&vec![1]).unwrap().blockchain.get_count(),
            2,
            "genesis + B: A rolled back"
        );

        // Pool: LOCKSTEP — A's delta reverted, B's delta intact.
        assert_eq!(
            pool.leaf_count(),
            3,
            "two seed leaves + B's leaf (A's leaf reverted)"
        );
        assert!(
            !pool.nullifiers().contains(null_a),
            "A's nullifier is unmarked (delta reverted)"
        );
        assert!(
            pool.nullifiers().contains(null_b),
            "B's nullifier is marked"
        );
        assert_eq!(
            pool.applied_count(),
            3,
            "two seed deltas + B's delta (A's delta reverted)"
        );
        assert_eq!(
            pool.current_root(),
            pool_tree_root(&pool),
            "post-rollback root history is coherent"
        );
    }

    // --- e2e test fixtures --------------------------------------------------

    /// The shielded token the e2e universe targets: self-verified + opt-in.
    fn make_e2e_token() -> Token {
        let mut t = Token::new();
        t.id = vec![1];
        t.is_self_verified = true;
        t.set_metadata("shielded_opt_in".into(), "true".into());
        t
    }

    fn e2e_seed_delta(block_hash: Vec<u8>, leaves: Vec<[u8; 32]>, post_root: [u8; 32])
    -> pneumatic_core::data::AppliedPoolDelta {
        pneumatic_core::data::AppliedPoolDelta {
            block_hash,
            leaves,
            nullifiers: vec![],
            post_root,
        }
    }

    /// The wire block for an e2e shielded commit: the canonical plain block
    /// shape (the committer's H12 tx-hash pairing is against the EMBEDDED
    /// plain transaction, byte-identical to what the finalizer embeds),
    /// carrying the shielded payload, chained off `prev_hash`.
    fn make_e2e_shielded_block(
        stx: &pneumatic_core::transactions::ShieldedTransaction,
        prev_hash: Vec<u8>,
    ) -> pneumatic_core::blocks::Block {
        let signed = SignedTransaction {
            shielded: Some(stx.clone()),
            transaction_id: stx.id.clone(),
            transaction: pneumatic_core::transactions::Transaction {
                id: stx.id.clone(),
                action: "ShieldedTransfer".into(),
                token_id: vec![1],
                bid: None,
                sequence_number: 1,
                sender: b"alice".to_vec(),
                receiver: vec![2],
                amount: None,
                timestamp: 0,
                result_hash: vec![],
                sender_signature: vec![],
            },
            total_voters: 3,
            total_stake: 42,
            leader_hash: prev_hash.clone(),
            leader_address: vec![],
            leader_stake: 0,
            finalizer_addr: vec![],
            finalizer_sig: pneumatic_core::transactions::TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![],
                current_stake: 0,
            },
            executor_sigs: std::collections::HashMap::new(),
            proposer_key: vec![],
        };
        let mut block = pneumatic_core::blocks::Block {
            signed_trans: signed,
            token_metadata: std::collections::HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        block.current_hash =
            BlockFactory::create_hash(&block).expect("well-formed test block hashes");
        block
    }

    fn e2e_commit(
        stx: &pneumatic_core::transactions::ShieldedTransaction,
        block: &pneumatic_core::blocks::Block,
    ) -> TransactionCommit {
        TransactionCommit {
            trans_id: stx.id.as_bytes().to_vec(),
            token_id: vec![1],
            env_id: "test_env".to_string(),
            proposed_block: block.clone(),
        }
    }

    /// The signed wire `Commit` message (envelope: the proposer's finalizer
    /// identity — registered as a Finalizer, which `Commit` permits).
    fn e2e_commit_message(
        commit: &TransactionCommit,
        identity: &NodeIdentity,
    ) -> Message {
        let body = serialize_to_bytes_rmp(commit).expect("commit serializes");
        Message::signed("env".to_string(), "Commit", body, None, identity).expect("signs")
    }

    /// Poll the recorder until a message with `action` appears (the
    /// fire-and-forget gossiper sends on a worker thread), then return it.
    async fn next_recorded(
        recorder: &Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        action: &str,
    ) -> Message {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (found, n) = {
                let guard = recorder.lock().unwrap();
                let found = guard
                    .iter()
                    .find(|raw| {
                        deserialize_rmp_to::<Message>(raw)
                            .map(|m| m.action == action)
                            .unwrap_or(false)
                    })
                    .cloned();
                (found, guard.len())
            };
            if let Some(raw) = found {
                return deserialize_rmp_to::<Message>(&raw).expect("recorded payload is a Message");
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for a recorded {action:?} (have {n} payloads)");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The pool's live tree root, rebuilt from the snapshot's leaf set (for
    /// the root-history coherence assertion) — `current_root()` is the
    /// history tip; the tree root over the same leaves must equal it.
    fn pool_tree_root(
        pool: &pneumatic_committer::shielded_pool::ShieldedPool,
    ) -> [u8; 32] {
        use pneumatic_core::shielded::{
            bytes_to_root, IncrementalMerkleTree, DEFAULT_DEPTH,
        };
        let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        for leaf in pool.state_snapshot().leaves {
            tree.append_leaf(&bytes_to_root(&leaf).expect("snapshot leaf is a committed leaf"));
        }
        pneumatic_core::shielded::root_to_bytes(&tree.root())
    }
}
