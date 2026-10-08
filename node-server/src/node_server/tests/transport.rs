//! Inbound transport + lifecycle tests: bridge data-plane routing,
//! foreign-action surfacing, non-stub finalizer inbound, dispatcher
//! preload routing, and the all-plugins shutdown fan-out.
use super::helpers::*;
use super::super::*;

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

/// Spy host: records the action it handled, on a TOKIO channel so the test
/// can await delivery without parking the (current-thread) test runtime.
struct DispatchSpy {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl RoleHandler for DispatchSpy {
    fn role(&self) -> NodeRegistryType {
        NodeRegistryType::Committer
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        &["Spy"]
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        Box::pin(async move {
            let _ = tx.send(message.action);
            Ok(())
        })
    }
}

impl RoleHost for DispatchSpy {
    fn advance_epoch(&mut self, _epoch: u64) {}
    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>>
    {
        Box::pin(async {})
    }
}

/// The bridge's data-plane dispatch must survive its REAL calling context:
/// the `on_packet` callback runs on RNS inbound worker threads — plain
/// `std::thread`s with **no ambient reactor**. The 10/08/2026 multi-host
/// rehearsal paid for this fact in a dead mesh: the bridge spawned with bare
/// ambient `tokio::spawn`, so the first data-plane frame of a tx burst
/// panicked every receiving worker ("there is no reactor running"), their
/// inbound queues closed, and each peer's directories evicted themselves —
/// `pneumatic_node_peers` 12 → 0 within one eviction window, while the sender
/// saw nothing. A handle captured inside the host runtime spawns from any
/// thread; reverting `spawn_data_dispatch` to ambient `tokio::spawn` fails
/// this test at the thread join, with the rehearsal's own panic.
#[tokio::test]
async fn bridge_data_dispatch_survives_a_runtimeless_caller() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let dispatcher = Arc::new(TokioMutex::new(RoleDispatcher::new(vec![Box::new(
        DispatchSpy { tx },
    )])));
    let data = serialize_to_bytes_rmp(&msg("Spy")).expect("frame serializes");

    // Capture the handle exactly as `build_runtime` does — inside the runtime.
    let rt = tokio::runtime::Handle::current();

    // Call it from a bare std thread: no runtime ambient there, same as an
    // RNS inbound worker.
    let caller = std::thread::spawn(move || {
        spawn_data_dispatch(&rt, dispatcher, data);
    });
    caller.join().expect(
        "data dispatch panicked on a runtime-free thread — the bridge is using \
         ambient `tokio::spawn` again; RNS workers have no reactor (10/08/2026 \
         mesh-killer: the panic closed the worker's inbound queue and evicted \
         the whole mesh)",
    );

    // The spawn must have reached the runtime, not evaporated with the thread:
    // the spy receives the action within 2 s.
    match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
        Ok(Some(action)) => assert_eq!(action, "Spy", "spy received the wrong action"),
        Ok(None) => panic!("spy channel closed with no action — dispatch never reached the role"),
        Err(_) => panic!("no action within 2 s — the dispatch was dropped or deadlocked"),
    }
}

/// The composite's COMMITTER_ACTIONS must cover every action
/// `Committer::handle_message` dispatches. A narrowed list silently deletes
/// the gossip half of the pipeline: in the 10/08/2026 multi-host rehearsal
/// the mesh formed, txs flowed, finalizers finalized — and the chain grew by
/// 0 of 200 because `BlockFinalized` (the gossip that CARRIES the finalized
/// block) was refused as `UnknownAction` before the committer handler could
/// authenticate and apply it. The adapter delegates straight to the real
/// dispatcher, so anything `handle_message` owns must reach it: refuse only
/// at the handler's own fail-closed auth, never at the composite's door.
#[tokio::test]
async fn committer_gossip_actions_reach_the_handler() {
    let cfg =
        runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Committer));
    let provider = Arc::new(MapStakeProvider::with_default(2000));
    let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

    for action in [
        "BlockFinalized",
        "BlockConfirmed",
        "BlockQuorumReached",
        "DistributeBlock",
        "DistributeToken",
        "EpochReconcile",
    ] {
        let outcome = server.dispatch(msg(action)).await;
        assert!(
            !matches!(outcome, Err(RoleError::UnknownAction(_))),
            "gossip action {action:?} was refused as UnknownAction — COMMITTER_ACTIONS has \
             drifted from Committer::handle_message (this exact drift committed 0 of 200 txs \
             in the 10/08/2026 rehearsal)"
        );
    }
}

/// `fact-composite-fanout-role-collision`, composite side: the finalizer-bound
/// preload rides the wire as `"PreloadForFinalizer"` (the executor's stamped-tx
/// hop and the sentinel's pre-notify both use it) and the dispatcher must ADMIT
/// it to the finalizer. In the 10/08/2026 rehearsal the hop shared the
/// executor's `"Preload"` name, so a multi-role host handed the finalizer's
/// frames to the executor adapter, no registry entry materialized, and every
/// `Sign` died `TransactionNotInFinalizing` — 0 of 200 committed. Removing the
/// action from `FINALIZER_ACTIONS` fails this at the `UnknownAction` arm
/// (mutation-verified).
#[tokio::test]
async fn finalizer_preload_action_reaches_the_handler() {
    let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Finalizer));
    let provider = Arc::new(MapStakeProvider::with_default(2000));
    let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

    let outcome = server.dispatch(msg("PreloadForFinalizer")).await;
    assert!(
        !matches!(outcome, Err(RoleError::UnknownAction(_))),
        "the finalizer-bound preload action was refused as UnknownAction — the hop that \
         materializes the finalizer's registry entry would never reach handle_preload, and \
         every Sign would die TransactionNotInFinalizing exactly as in the 10/08/2026 \
         multi-host rehearsal"
    );
}

/// The invariant the collision broke, stated statically: `RoleDispatcher`
/// admits an action to exactly ONE installed role (2+ owners is
/// `AmbiguousAction`), and a name owned by no installed role is `UnknownAction`.
/// So a wire name shared by two roles' inbound sets is a latent misroute on
/// every multi-role host — exactly how the executor's `"Preload"` swallowed
/// the finalizer's preload frames. The four admit-lists MUST stay pairwise
/// disjoint. (Mutation: add `"Preload"` to `FINALIZER_ACTIONS` and this fails.)
#[test]
fn role_action_sets_are_pairwise_disjoint() {
    use crate::node_server::{
        COMMITTER_ACTIONS, EXECUTOR_ACTIONS, FINALIZER_ACTIONS, SENTINEL_ACTIONS,
    };
    let sets = [
        ("Committer", COMMITTER_ACTIONS),
        ("Executor", EXECUTOR_ACTIONS),
        ("Sentinel", SENTINEL_ACTIONS),
        ("Finalizer", FINALIZER_ACTIONS),
    ];
    for (i, (name_a, set_a)) in sets.iter().enumerate() {
        for (name_b, set_b) in sets.iter().skip(i + 1) {
            for action in set_a.iter() {
                assert!(
                    !set_b.contains(action),
                    "action {action:?} is owned by BOTH {name_a} and {name_b} — a composite \
                     host running both roles can serve only one and silently misroutes the \
                     other's frames (fact-composite-fanout-role-collision)"
                );
            }
        }
    }
}
