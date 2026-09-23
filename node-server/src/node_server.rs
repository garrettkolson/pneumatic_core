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
}

// Re-exported so the pre-split paths `node_server::build_runtime` /
// `node_server::build_role_plugin` / `node_server::route_data_plane`
// resolve unchanged (the children own the definitions now).
pub use self::build::build_runtime;
pub(crate) use self::plugins::build_role_plugin;
pub(crate) use self::transport::route_data_plane;

pub mod build;
pub mod epoch_coord;
pub mod plugins;
pub mod role_adapters;
pub mod transport;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    pub mod helpers;
    mod build;
    mod e2e;
    mod epoch;
    mod shielded;
    mod transport;
}
