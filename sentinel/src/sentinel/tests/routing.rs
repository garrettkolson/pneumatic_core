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

/// One transaction, one decision.
///
/// Every host in the composite rehearsal ran a Sentinel, each admitted the same gossiped
/// transaction, and each stamped its own decision for its own host — so the environment
/// held four routing decisions for one transaction, four Finalizers each built a block for
/// the one it had been handed, and the Committer had to choose between them
/// (`fact-composite-per-host-fork-divergence`). ADR-007 puts routing authority in the
/// Sentinel that admits a transaction; a second Sentinel selecting for itself is a second
/// authority claiming the same transaction.
///
/// The A/B is the fixture itself. This Sentinel has no stake snapshot, so a transaction
/// that asks it to route fails closed — it has no defensible answer to "who finalizes
/// this". The same transaction arriving already routed does not, and the record it gets
/// back is the one that arrived, byte for byte: the attestation of a *different* key,
/// which this node could not have produced. A re-attestation with this node's own key
/// would be the rehearsal's bug wearing a fresh signature, so the comparison is of the
/// whole record rather than just the Finalizer key.
#[test]
fn a_decision_that_arrives_with_the_transaction_is_honoured_not_recomputed() {
    let sentinel = {
        let (sentinel, _registry) = super::helpers::make_sentinel_fixture();
        sentinel
    };
    let token = pneumatic_core::tokens::Token::new();

    let mut tx = Transaction {
        payload: vec![],
        gas_limit: 0,
        id: "one_decision_tx".into(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender: vec![1],
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
        result_data: vec![],
        selection: None,
    };
    let carried = {
        let attester = NodeIdentity::generate_in_memory();
        pneumatic_core::transactions::SelectionRecord::attest(
            &attester.ed25519,
            "one_decision_tx",
            pneumatic_core::epoch::FINALIZER_DOMAIN,
            vec![0xC0; 32],
            1,
            vec![0x5A; 32],
        )
        .expect("the first Sentinel attests its decision")
    };

    // Control: no snapshot and nothing carried, so there is nothing to route from.
    let unrouted = sentinel
        .routing_decision_for(&tx, &token)
        .expect_err("no snapshot and no carried decision means no decision");
    assert!(
        matches!(unrouted, SentinelError::Routing(_)),
        "the premise of the second half is that this fixture cannot route on its own: {unrouted:?}"
    );

    // Same Sentinel, same fixture, transaction already routed.
    tx.selection = Some(carried.clone());
    let (finalizer_key, record) = sentinel
        .routing_decision_for(&tx, &token)
        .expect("a transaction that arrives routed is admitted with the decision it carries");

    assert_eq!(finalizer_key, vec![0xC0; 32], "the routed Finalizer is the one the decision names");
    assert_eq!(
        record, carried,
        "the record must be the one that arrived — including another node's attestation"
    );
    assert_ne!(
        record.sentinel_public_key,
        sentinel
            .node_registry
            .get_config()
            .public_key
            .clone(),
        "and it must not have been re-attested by this node"
    );
}
