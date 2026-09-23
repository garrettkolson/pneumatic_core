//! Epoch-coordinator tests: advance fan-out to all roles,
//! poll-triggered advance, and role-set recomputation.
use super::helpers::*;
use super::super::*;

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
