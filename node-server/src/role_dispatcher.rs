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
    handlers: Vec<Box<dyn RoleHandler>>,
}

impl RoleDispatcher {
    /// Build a dispatcher over the installed role plugins.
    pub fn new(handlers: Vec<Box<dyn RoleHandler>>) -> Self {
        Self { handlers }
    }

    /// The roles currently installed, in registration order.
    pub fn installed_roles(&self) -> Vec<NodeRegistryType> {
        self.handlers.iter().map(|h| h.role()).collect()
    }

    /// Route one inbound message to the single installed role that owns its
    /// `action`, fail-closed otherwise.
    pub async fn dispatch(&self, message: Message) -> Result<(), RoleError> {
        let action = message.action.clone();
        let matches: Vec<&dyn RoleHandler> = self
            .handlers
            .iter()
            .filter(|h| h.allowed_actions().iter().any(|a| *a == action.as_str()))
            .map(|h| &**h)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Test double for `RoleHandler`. Records every action it handled into a
    /// shared spy so a test can assert the message landed on the right role.
    struct SpyHandler {
        role: NodeRegistryType,
        actions: &'static [&'static str],
        spy: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }

    impl RoleHandler for SpyHandler {
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

    fn spy(role: NodeRegistryType, actions: &'static [&'static str], fail: bool) -> (Box<dyn RoleHandler>, Arc<Mutex<Vec<String>>>) {
        let spy = Arc::new(Mutex::new(Vec::new()));
        (Box::new(SpyHandler { role, actions, spy: spy.clone(), fail }), spy)
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
        let (c, spy_c) = spy(NodeRegistryType::Committer, &["Commit"], false);
        let (s, _spy_s) = spy(NodeRegistryType::Sentinel, &["Verify"], false);
        let d = RoleDispatcher::new(vec![c, s]);

        // "Preload" is owned by Executor, which is NOT installed → rejected.
        assert!(matches!(d.dispatch(msg("Preload")).await, Err(RoleError::UnknownAction(_))));

        // "Commit" IS owned by the installed Committer → routed + recorded.
        assert!(d.dispatch(msg("Commit")).await.is_ok());
        assert_eq!(spy_c.lock().unwrap().clone(), vec!["Committer:Commit".to_string()]);
    }

    #[tokio::test]
    async fn routes_preload_only_when_executor_installed() {
        let (c, spy_c) = spy(NodeRegistryType::Committer, &["Commit"], false);
        let (e, spy_e) = spy(NodeRegistryType::Executor, &["Preload"], false);
        let d = RoleDispatcher::new(vec![c, e]);

        assert!(d.dispatch(msg("Preload")).await.is_ok());
        assert_eq!(spy_e.lock().unwrap().clone(), vec!["Executor:Preload".to_string()]);
        // Committer must stay untouched when only Preload arrives.
        assert!(spy_c.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_unknown_action() {
        let (c, _spy) = spy(NodeRegistryType::Committer, &["Commit"], false);
        let d = RoleDispatcher::new(vec![c]);

        // No installed role owns "Confirm" → fail closed with the action name.
        match d.dispatch(msg("Confirm")).await {
            Err(RoleError::UnknownAction(a)) => assert_eq!(a, "Confirm"),
            other => panic!("expected UnknownAction, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn single_role_routing() {
        let (c, spy_c) = spy(NodeRegistryType::Committer, &["Commit"], false);
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
        let (a, _) = spy(NodeRegistryType::Committer, &["Commit"], false);
        let (b, _) = spy(NodeRegistryType::Sentinel, &["Commit"], false);
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
        let (c, _) = spy(NodeRegistryType::Committer, &["Commit"], true);
        let d = RoleDispatcher::new(vec![c]);

        match d.dispatch(msg("Commit")).await {
            Err(RoleError::Downstream(PneumaticError::Network(msg))) => assert_eq!(msg, "role handler error"),
            other => panic!("expected the downstream error to wrap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn installed_roles_reflect_registration_order() {
        let (c, _) = spy(NodeRegistryType::Committer, &["Commit"], false);
        let (e, _) = spy(NodeRegistryType::Executor, &["Preload"], false);
        let d = RoleDispatcher::new(vec![c, e]);
        assert_eq!(d.installed_roles(), vec![NodeRegistryType::Committer, NodeRegistryType::Executor]);
    }
}
