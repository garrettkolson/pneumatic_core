//! In-process inbound router backbone. Routes an inbound `Message` by action to
//! the single installed role that owns that action, fail-closed on unknown
//! actions. Implemented in Phase 2.
//!
//! This is the fresh in-process dispatch layer — deliberately *not* the dead
//! `pneumatic_core::server::ThreadPool` and *not* RNS (which is only the external
//! inter-node wire). It is a thin router: it inspects only the `action` string,
//! never the body, and forwards to the matching role plugin instead of
//! re-validating token coordination (that is `ActionRouter`'s job upstream).

use pneumatic_core::errors::PneumaticError;
use pneumatic_core::messages::Message;
use pneumatic_core::node::NodeRegistryType;

/// Errors produced while routing an inbound message to an installed role.
#[derive(Debug)]
pub enum RoleError {
    /// No installed role owns this action. Fail closed: the message is logged
    /// and never silently dropped (mirrors the registration/action gate ethos).
    UnknownAction(String),
    /// More than one installed role claims the action — a wiring bug; fail
    /// closed rather than silently picking one.
    AmbiguousAction {
        action: String,
        roles: Vec<NodeRegistryType>,
    },
    /// The matching role's own handler returned a protocol-level error.
    Downstream(PneumaticError),
}

impl std::fmt::Display for RoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoleError::UnknownAction(a) => write!(f, "unknown action: {a:?}"),
            RoleError::AmbiguousAction { action, roles } => write!(
                f,
                "ambiguous action {action:?} owned by multiple installed roles: {roles:?}"
            ),
            RoleError::Downstream(e) => write!(f, "role handler error: {e}"),
        }
    }
}

impl std::error::Error for RoleError {}

impl From<PneumaticError> for RoleError {
    fn from(e: PneumaticError) -> Self {
        RoleError::Downstream(e)
    }
}

/// A role-plugin installed in the composite host. It owns exactly one registry
/// type, a fixed set of inbound `action` strings, and an async `handle` for
/// messages of those actions. `Send + Sync` so an erased `Box<dyn RoleHandler>`
/// can be awaited across `.await` points (and spawned).
///
/// `handle` returns an explicitly `Send`-boxed future rather than a native
/// `async fn`: native async fns in `dyn` traits are not `Send`-posable, but an
/// installed-role Gossiper handler forwards to `RoleDispatcher::dispatch` inside
/// `tokio::spawn`, which requires the future to cross a thread boundary. Boxed
/// `Future + Send` is the dependency-free way to make `dyn RoleHandler` `Send`.
pub trait RoleHandler: Send + Sync {
    /// The registry type this role-plugin installs under.
    fn role(&self) -> NodeRegistryType;

    /// The inbound actions this role owns. A message whose `action` is in this
    /// set is routed here; anything else is dropped by the router (never by us).
    fn allowed_actions(&self) -> &'static [&'static str];

    /// Handle one inbound message the router has selected for this role.
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>>;
}

/// Lifecycle for an installed role-plugin: the two things the composite's epoch
/// coordinator drives after a message has already been routed by `RoleHandler`
/// (Phase 5). Kept on a *separate* trait from `RoleHandler` on purpose — the
/// two concerns differ in lifetime (epoch fan-out is synchronous; shutdown is
/// async) and both are needed to run a node through its full lifecycle.
///
/// The coordinator drives these over `Vec<Box<dyn RoleHost>>` via `iter_mut`:
/// that boxed handle supplies the mutable access a `&mut self` `advance_epoch`
/// (the Finalizer's) needs, so no `Mutex` re-wrap of the plugin is required.
/// Each crate supplies this impl alongside its `impl RoleHandler` from Phase 2/4;
/// `role()`/`allowed_actions()`/`handle` are satisfied by that other impl.
pub trait RoleHost: RoleHandler {
    /// Advance the role to `epoch`. Roles that self-drive their epoch (the
    /// Committer, via its own `run_epoch_loop`) no-op here; the roles advanced
    /// from an external epoch signal (Sentinel, Finalizer) bump their internal
    /// state. Visited for *every* installed role — the Committer's no-op is
    /// still part of the fan-out.
    fn advance_epoch(&mut self, epoch: u64);

    /// Begin graceful shutdown. Roles without a shutdown lifecycle (Sentinel,
    /// Executor) implement this as an empty no-op; Committer + Finalizer run
    /// their real shutdown. Boxed `Future + Send` rather than native `async fn`
    /// so the method is `Send`-posable into an erased `dyn` return (mirrors the
    /// boxed `RoleHandler::handle`). `'a` is the shared borrow of `&mut self`,
    /// so a returned future may borrow the host for the whole borrow.
    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
    >;
}

/// The in-process inbound router between the RNS bridge and the installed role
/// plugins.
///
/// Routing policy, all fail-closed:
/// - `0` matching handlers → `RoleError::UnknownAction` (logged upstream, never
///   silently dropped).
/// - `1` matching handler → its `handle` is awaited.
/// - `2+` matching handlers → `RoleError::AmbiguousAction` (wiring bug, never
///   silently resolved by picking one).
#[derive(Default)]
pub struct RoleDispatcher {
    // Stored as `RoleHost` (Phase 5): every installed plugin is a `RoleHandler`
    // (inbound routing) and a `RoleHost` (epoch fan-out + shutdown), so the
    // dispatcher owns a single handle per role that serves both concerns.
    hosts: Vec<Box<dyn RoleHost>>,
}

impl RoleDispatcher {
    /// Build a dispatcher over the installed role plugins.
    pub fn new(hosts: Vec<Box<dyn RoleHost>>) -> Self {
        Self { hosts }
    }

    /// The roles currently installed, in registration order.
    pub fn installed_roles(&self) -> Vec<NodeRegistryType> {
        self.hosts.iter().map(|h| h.role()).collect()
    }

    /// Route one inbound message to the single installed role that owns its
    /// `action`, fail-closed otherwise.
    pub async fn dispatch(&self, message: Message) -> Result<(), RoleError> {
        let action = message.action.clone();
        let matches: Vec<&dyn RoleHandler> = self
            .hosts
            .iter()
            .filter(|h| h.allowed_actions().iter().any(|a| *a == action.as_str()))
            // Upcast each `&dyn RoleHost` to `&dyn RoleHandler` (RoleHost:
            // RoleHandler) so the matches can be awaited through the inbound
            // role interface — dispatch never needs the lifecycle handle.
            .map(|h| &**h as &dyn RoleHandler)
            .collect();

        match matches.len() {
            0 => Err(RoleError::UnknownAction(action)),
            1 => matches[0].handle(message).await,
            _ => Err(RoleError::AmbiguousAction {
                action,
                roles: matches.iter().map(|h| h.role()).collect(),
            }),
        }
    }

    /// Fan one epoch advance to *every* installed role — the coordinator's
    /// `roll_forward` on an epoch boundary. Each installed `advance_epoch` is
    /// visited (the Committer's is a no-op that self-drives); the roles visited
    /// are returned so callers/tests can observe the full fan-out. `&mut`
    /// because `advance_epoch` needs `&mut self` (the Finalizer's).
    pub fn roll_forward(&mut self, epoch: u64) -> Vec<NodeRegistryType> {
        let mut advanced = Vec::new();
        for host in self.hosts.iter_mut() {
            host.advance_epoch(epoch);
            advanced.push(host.role());
        }
        advanced
    }

    /// Fan graceful shutdown to every installed role (Phase 5 shutdown path).
    /// Roles without a shutdown lifecycle no-op. `&mut` for the same reason as
    /// `roll_forward`.
    pub async fn initiate_all_shutdown(&mut self) {
        for host in self.hosts.iter_mut() {
            host.initiate_shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Test double for `RoleHost`. Records every inbound action, every epoch
    /// fan-out, and every shutdown into a shared spy so a test can assert on
    /// message routing *and* on the Phase-5 lifecycle fan-outs.
    struct SpyHost {
        role: NodeRegistryType,
        actions: &'static [&'static str],
        spy: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }

    impl RoleHandler for SpyHost {
        fn role(&self) -> NodeRegistryType {
            self.role.clone()
        }

        fn allowed_actions(&self) -> &'static [&'static str] {
            self.actions
        }

        fn handle<'a>(
            &'a self,
            message: Message,
        ) -> std::pin::Pin<std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>> {
            let spy = self.spy.clone();
            let role = self.role.clone();
            let fail = self.fail;
            Box::pin(async move {
                spy.lock().unwrap().push(format!("{:?}:{}", role, message.action));
                if fail {
                    Err(RoleError::Downstream(PneumaticError::Network("role handler error".to_string())))
                } else {
                    Ok(())
                }
            })
        }
    }

    // `advance_epoch` + `initiate_shutdown` are the lifecycle methods; they are a
    // separate impl from RoleHandler so RoleHost (which extends RoleHandler) is
    // satisfied by two impls rather than one bloated one.
    impl RoleHost for SpyHost {
        fn advance_epoch(&mut self, epoch: u64) {
            self.spy
                .lock()
                .unwrap()
                .push(format!("{:?}:advance_epoch:{}", self.role, epoch));
        }

        fn initiate_shutdown<'a>(
            &'a mut self,
        ) -> std::pin::Pin<
            std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
        > {
            let spy = self.spy.clone();
            let role = self.role.clone();
            Box::pin(async move {
                spy.lock().unwrap().push(format!("{:?}:shutdown", role));
            })
        }
    }

    fn spy_host(role: NodeRegistryType, actions: &'static [&'static str], fail: bool) -> (Box<dyn RoleHost>, Arc<Mutex<Vec<String>>>) {
        let spy = Arc::new(Mutex::new(Vec::new()));
        (Box::new(SpyHost { role, actions, spy: spy.clone(), fail }), spy)
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

    #[tokio::test]
    async fn routes_to_installed_role_only() {
        // Committer owns "Commit", Sentinel owns "Verify" — no Executor yet.
        let (c, spy_c) = spy_host(NodeRegistryType::Committer, &["Commit"], false);
        let (s, _spy_s) = spy_host(NodeRegistryType::Sentinel, &["Verify"], false);
        let d = RoleDispatcher::new(vec![c, s]);

        // "Preload" is owned by Executor, which is NOT installed → rejected.
        assert!(matches!(d.dispatch(msg("Preload")).await, Err(RoleError::UnknownAction(_))));

        // "Commit" IS owned by the installed Committer → routed + recorded.
        assert!(d.dispatch(msg("Commit")).await.is_ok());
        assert_eq!(spy_c.lock().unwrap().clone(), vec!["Committer:Commit".to_string()]);
    }

    #[tokio::test]
    async fn routes_preload_only_when_executor_installed() {
        let (c, spy_c) = spy_host(NodeRegistryType::Committer, &["Commit"], false);
        let (e, spy_e) = spy_host(NodeRegistryType::Executor, &["Preload"], false);
        let d = RoleDispatcher::new(vec![c, e]);

        assert!(d.dispatch(msg("Preload")).await.is_ok());
        assert_eq!(spy_e.lock().unwrap().clone(), vec!["Executor:Preload".to_string()]);
        // Committer must stay untouched when only Preload arrives.
        assert!(spy_c.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_unknown_action() {
        let (c, _spy) = spy_host(NodeRegistryType::Committer, &["Commit"], false);
        let d = RoleDispatcher::new(vec![c]);

        // No installed role owns "Confirm" → fail closed with the action name.
        match d.dispatch(msg("Confirm")).await {
            Err(RoleError::UnknownAction(a)) => assert_eq!(a, "Confirm"),
            other => panic!("expected UnknownAction, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn single_role_routing() {
        let (c, spy_c) = spy_host(NodeRegistryType::Committer, &["Commit"], false);
        let d = RoleDispatcher::new(vec![c]);

        // The one installed role handles its action…
        assert!(d.dispatch(msg("Commit")).await.is_ok());
        assert_eq!(spy_c.lock().unwrap().len(), 1);
        // …and a foreign action from that same role is rejected, never routed.
        assert!(d.dispatch(msg("Verify")).await.is_err());
    }

    #[tokio::test]
    async fn ambiguous_action_is_rejected_not_resolved() {
        // Two installed roles both claim "Commit" — a wiring bug; the router
        // must fail closed rather than silently pick one.
        let (a, _) = spy_host(NodeRegistryType::Committer, &["Commit"], false);
        let (b, _) = spy_host(NodeRegistryType::Sentinel, &["Commit"], false);
        let d = RoleDispatcher::new(vec![a, b]);

        match d.dispatch(msg("Commit")).await {
            Err(RoleError::AmbiguousAction { action, roles }) => {
                assert_eq!(action, "Commit");
                assert_eq!(roles.len(), 2, "ambiguous action must name both owners");
            }
            other => panic!("expected AmbiguousAction, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn downstream_handler_error_propagates() {
        let (c, _) = spy_host(NodeRegistryType::Committer, &["Commit"], true);
        let d = RoleDispatcher::new(vec![c]);

        match d.dispatch(msg("Commit")).await {
            Err(RoleError::Downstream(PneumaticError::Network(msg))) => assert_eq!(msg, "role handler error"),
            other => panic!("expected the downstream error to wrap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn installed_roles_reflect_registration_order() {
        let (c, _) = spy_host(NodeRegistryType::Committer, &["Commit"], false);
        let (e, _) = spy_host(NodeRegistryType::Executor, &["Preload"], false);
        let d = RoleDispatcher::new(vec![c, e]);
        assert_eq!(d.installed_roles(), vec![NodeRegistryType::Committer, NodeRegistryType::Executor]);
    }

    /// The coordinator's `roll_forward` fans `advance_epoch` to *every* installed
    /// host — a no-op revert (advancing nothing) records no `advance_epoch`
    /// entries, so this fails. Fails if the fan-out skips a role.
    #[tokio::test]
    async fn roll_forward_fans_to_all_hosts() {
        let (c, spy_c) = spy_host(NodeRegistryType::Committer, &["Commit"], false);
        let (s, spy_s) = spy_host(NodeRegistryType::Sentinel, &["Verify"], false);
        let (e, spy_e) = spy_host(NodeRegistryType::Executor, &["Preload"], false);
        let (f, spy_f) = spy_host(NodeRegistryType::Finalizer, &["Sign"], false);
        let mut d = RoleDispatcher::new(vec![c, s, e, f]);

        let advanced = d.roll_forward(5);
        // Visited every installed role (registration order) …
        assert_eq!(advanced, d.installed_roles());
        // … and each host's `advance_epoch(5)` ran (Committer's is a no-op, but
        // it is still visited — the spy records the visit).
        assert_eq!(
            spy_c.lock().unwrap().clone(),
            vec!["Committer:advance_epoch:5".to_string()]
        );
        assert_eq!(
            spy_s.lock().unwrap().clone(),
            vec!["Sentinel:advance_epoch:5".to_string()]
        );
        assert_eq!(
            spy_e.lock().unwrap().clone(),
            vec!["Executor:advance_epoch:5".to_string()]
        );
        assert_eq!(
            spy_f.lock().unwrap().clone(),
            vec!["Finalizer:advance_epoch:5".to_string()]
        );
    }

    /// `initiate_all_shutdown` fans shutdown to every installed host. A no-op
    /// revert records no `shutdown` entries ⇒ the assert fails.
    #[tokio::test]
    async fn initiate_all_shutdown_fans_to_all_hosts() {
        let (c, spy_c) = spy_host(NodeRegistryType::Committer, &["Commit"], false);
        let (s, spy_s) = spy_host(NodeRegistryType::Sentinel, &["Verify"], false);
        let (e, spy_e) = spy_host(NodeRegistryType::Executor, &["Preload"], false);
        let (f, spy_f) = spy_host(NodeRegistryType::Finalizer, &["Sign"], false);
        let mut d = RoleDispatcher::new(vec![c, s, e, f]);

        d.initiate_all_shutdown().await;

        assert_eq!(
            spy_c.lock().unwrap().clone(),
            vec!["Committer:shutdown".to_string()]
        );
        assert_eq!(
            spy_s.lock().unwrap().clone(),
            vec!["Sentinel:shutdown".to_string()]
        );
        assert_eq!(
            spy_e.lock().unwrap().clone(),
            vec!["Executor:shutdown".to_string()]
        );
        assert_eq!(
            spy_f.lock().unwrap().clone(),
            vec!["Finalizer:shutdown".to_string()]
        );
    }
}
