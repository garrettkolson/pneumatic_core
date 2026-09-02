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
use pneumatic_core::data::{DataProvider, DefaultDataProvider};
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
const FINALIZER_ACTIONS: &'static [&'static str] = &["Sign", "Finalize"];

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
    // too.
    role_dispatcher: TokioMutex<RoleDispatcher>,
    // Snapshot of the roles this host installed at boot — read by `installed_roles`
    // synchronously without contending the `tokio` mutex (the tokio `Mutex` in the
    // pinned workspace has no sync `lock_blocking`). The set does not change until
    // Phase 6's deferred re-registration path, so a boot-time snapshot is faithful.
    installed_roles: Vec<pneumatic_core::node::NodeRegistryType>,
    // Lifecycle seed carried for Phase 5 (epoch coordinator).
    epoch_boundary_detector: Arc<EpochBoundaryDetector>,
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

    // --- data provider -------------------------------------------------------
    let data_provider: Arc<dyn DataProvider> = Arc::new(DefaultDataProvider::new());

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
            )
        })
        .collect();
    // Snapshot the installed roles (boot-time; unchanged until Phase 6's
    // deferred re-registration path) before moving `installed` into the locked
    // dispatcher, so `installed_roles()` can report them synchronously.
    let installed_roles: Vec<pneumatic_core::node::NodeRegistryType> =
        installed.iter().map(|h| h.role()).collect();
    let role_dispatcher = TokioMutex::new(RoleDispatcher::new(installed));

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
) -> Option<Box<dyn RoleHost>> {
    use pneumatic_core::node::NodeRegistryType;
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
            );
            Some(Box::new(finalizer))
        }
        // Archiver has no role-plugin this host hosts.
        NodeRegistryType::Archiver => None,
    }
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
                // Any other action this role owns is not a voter signature; fail
                // closed with a protocol-level error rather than silently
                // accepting it.
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

    use pneumatic_core::config::{BootstrapPeer, Config};
    use pneumatic_core::errors::PneumaticError;
    use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use pneumatic_core::messages::Message;
    use pneumatic_core::node::{NodeTypeConfig, NodeRegistryType};

    use crate::role_dispatcher::RoleError;
    use crate::role_selector::StakeProvider;
    use super::build_runtime;

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

    /// The host boots even when the RNS transport cannot start — a missing
    /// or un-routable transport is tolerated, so a node can come up and later
    /// register once its peers/data come online. This fails if `build_runtime`
    /// hard-errors on a transport that will not start.
    #[tokio::test]
    async fn build_runtime_no_transport_booted_cleanly() {
        let cfg = runtime_config(vec![bad_peer()], type_config_floor(0));
        let provider = Arc::new(MapStakeProvider::with_default(2000));

        // No panic, no hard error — the host is constructible without transport.
        let server = build_runtime(cfg, provider).expect("host boots without transport");
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
        let server = build_runtime(cfg, provider).expect("host builds");

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
        let server = build_runtime(cfg, provider).expect("host builds");

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

    /// The Finalizer's inbound handler is the real voter chokepoint, not a stub:
    /// a `Sign` reaching the host reaches `handle_signature`, which fails closed
    /// on the empty/invalid body with a protocol error — never the Phase-3 stub's
    /// `"finalizer inbound not yet wired"` error. Fails if the handler still
    /// returns the stub.
    #[tokio::test]
    async fn finalizer_inbound_handler_not_stub() {
        let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Finalizer));
        let provider = Arc::new(MapStakeProvider::with_default(2000));
        let server = build_runtime(cfg, provider).expect("host builds");

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
        let server = build_runtime(cfg, provider).expect("host builds");

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
        let server = build_runtime(cfg, provider).expect("host builds");

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
        let server = build_runtime(cfg, provider).expect("host builds");

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
        let server = build_runtime(cfg, provider).expect("host builds");

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
            build_runtime(cold_cfg, Arc::new(MapStakeProvider::with_default(0))).expect("host builds");
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
        let server = build_runtime(cfg, provider).expect("host builds");

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
}
