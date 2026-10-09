//! ADR-004 candidate set at the Sentinel: the epoch stake snapshot decides *weight*,
//! and only Finalizers this node can address are candidates at all.
//!
//! The snapshot is a set of staked keys, and a staked key is not necessarily a node
//! that runs a Finalizer — the same snapshot elects leaders and carries every staked
//! node in the environment. Selecting across all of it and then sending to exactly one
//! recipient produced transactions routed to executors and committers, which nothing
//! downstream could honor: the Finalizer bucket had no such node, the assigned
//! Finalizer never saw the transaction, and the enforcing roles were right to refuse
//! it. These tests pin the two halves of that — unreachable staked keys are never
//! chosen, and a snapshot with no reachable Finalizer is a loud failure rather than a
//! silent stall.

use super::helpers::*;
use super::super::*;
use pneumatic_core::epoch::StakeSet;

fn sentinel_with_snapshot(snapshot: StakeSet) -> Sentinel {
    let data_provider = StubDataProvider::new()
        .with_stake_snapshot(0, snapshot.clone())
        .with_stake_snapshot(1, snapshot);
    let (sentinel, _registry) =
        make_sentinel_fixture_with_env_and_data_provider(data_provider, make_test_env_data());
    sentinel
}

/// A staked key that runs no Finalizer must never be picked — even though it holds
/// most of the stake, so a weight-only selection would choose it far more often than
/// not.
#[test]
fn a_staked_key_that_is_not_a_finalizer_is_never_selected() {
    let reachable = vec![0xA1; 32];
    let unreachable = vec![0xB2; 32];
    let snapshot = StakeSet {
        stakers: [
            (unreachable.clone(), 900u64),
            (reachable.clone(), 100u64),
        ]
        .into_iter()
        .collect(),
    };
    let sentinel = sentinel_with_snapshot(snapshot);
    register_finalizers(&sentinel, &[reachable.clone()]);

    for i in 0..200 {
        let picked = sentinel
            .assign_finalizer_deterministic(&format!("tx_{i}"), 1, b"salt")
            .expect("one reachable finalizer is enough to route");
        assert_eq!(
            picked, reachable,
            "tx_{i} was routed to a key that is not a Finalizer this sentinel can reach"
        );
    }
}

/// No reachable Finalizer is an error, with the numbers that explain it. The
/// alternative — falling back to the whole stake set — is the failure mode above,
/// dressed up as progress.
#[test]
fn a_snapshot_with_no_reachable_finalizer_refuses_to_route() {
    let snapshot = StakeSet {
        stakers: [(vec![0xB2; 32], 100u64)].into_iter().collect(),
    };
    let sentinel = sentinel_with_snapshot(snapshot);

    let err = sentinel
        .assign_finalizer_deterministic("tx_orphan", 1, b"salt")
        .expect_err("nothing reachable, so nothing may be assigned");
    let message = match &err {
        SentinelError::Routing(m) => m.clone(),
        other => panic!("expected a Routing error, got {other:?}"),
    };
    assert!(
        message.contains("1 staked keys") && message.contains("0 registered Finalizers"),
        "the error has to say what it looked at, got: {message}"
    );
}

/// The single-host case, which on a composite is the only case.
///
/// A host's own roles are never in its own peer bucket — peering registers *peers*, and
/// the same-host hop travels through self-delivery. A candidate set built from the
/// bucket alone would call a single-host deployment un-routable and refuse every
/// transaction on it, which is exactly what this guards.
#[test]
fn the_host_serving_a_finalizer_counts_as_reachable_for_its_own_key() {
    let mut sentinel = sentinel_with_snapshot(StakeSet { stakers: std::collections::HashMap::new() });
    // The fixture generates its own identity, so the own key is only known after the
    // build. Prime the epoch cache rather than the data provider: `get` is the read
    // path here, and the cache holds its own provider handle from construction.
    let own_key = sentinel.node_registry.get_config().public_key.clone();
    sentinel.stake_snapshot_cache.put(
        1,
        StakeSet {
            stakers: [(own_key.clone(), 100u64)].into_iter().collect(),
        },
    );
    // No peers registered at all: this host IS the Finalizer.
    sentinel
        .node_registry
        .install_self_delivery(vec![NodeRegistryType::Finalizer], std::sync::Arc::new(|_| {}));

    let picked = sentinel
        .assign_finalizer_deterministic("tx_self_host", 1, b"salt")
        .expect("a host that runs a Finalizer can route to it");
    assert_eq!(
        picked, own_key,
        "the own-key candidate is the one this host can actually deliver to"
    );
}
