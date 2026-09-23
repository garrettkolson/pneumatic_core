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
