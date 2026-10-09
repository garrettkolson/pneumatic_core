//! Same-host delivery (`self_delivery`): the composite's fan-out must reach the
//! roles this host runs, exactly once, through the same inbound path a wire
//! frame takes — and must never double for a host that does not run the target
//! role.
//!
//! The defect these tests exist for (`fact-composite-no-self-delivery`): every
//! pipeline hop is a fan-out to a role bucket, a bucket holds peers only, and on
//! a four-role composite every host *is* that role too. So a node's own copy of
//! its own `Commit` — the one copy whose block hash matches the booking its own
//! finalizer role made — never arrived, and no chain could ever grow. The
//! composite e2e suite could not see it because the harness relays each hop by
//! hand: the tests *were* the missing self-delivery.

use super::helpers::*;
use super::super::*;
use crate::node::NetworkPacket;
use super::super::self_delivery::PacketSink;

/// A sink that records every frame fed into the (fictional) inbound path.
fn recording_sink() -> (PacketSink, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
    let captured: Arc<std::sync::Mutex<Vec<Vec<u8>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink: PacketSink = {
        let captured = Arc::clone(&captured);
        Arc::new(move |frame: Vec<u8>| captured.lock().unwrap().push(frame))
    };
    (sink, captured)
}

/// The payload of the i-th frame the sink captured, decoded through the SAME
/// framing the transport applies — the assertion that a local copy is a
/// *wire-shaped frame*, not a privileged in-process shortcut.
fn captured_payload(captured: &Arc<std::sync::Mutex<Vec<Vec<u8>>>>, i: usize) -> Vec<u8> {
    let frames = captured.lock().unwrap();
    let packet: NetworkPacket = crate::encoding::deserialize_rmp_to(&frames[i])
        .expect("the local copy is a serialized NetworkPacket frame, exactly as the wire carries");
    packet.data.expect("a data-plane frame carries data")
}

/// Everything a `RecordingConnection` actually delivered. Both fan-out methods
/// complete their sends before returning, so a drain (not a poll) is enough.
fn drain(rx: &mut mpsc::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut sent = Vec::new();
    while let Ok(p) = rx.try_recv() {
        sent.push(p);
    }
    sent
}

fn payload() -> Vec<u8> {
    b"pipeline-hop-payload".to_vec()
}

/// The load-bearing case: a fan-out to a role this host runs delivers one copy
/// locally *and* still sends to its peers.
#[test]
fn a_fanout_to_a_role_this_host_runs_delivers_exactly_one_local_copy() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let (sink, captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Finalizer], sink);

    // One remote finalizer, so the fan-out has a peer path to exercise too.
    let (tx, mut rx) = mpsc::channel(16);
    assert!(
        reg.register_peer(
            vec![0xA1; 32],
            [1u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { tx }),
        ),
        "the remote finalizer must be admitted"
    );

    reg.send_to_all_blocking(payload(), &NodeRegistryType::Finalizer);

    assert_eq!(
        captured.lock().unwrap().len(),
        1,
        "the host is a member of the role it fans out to — exactly one local copy"
    );
    assert_eq!(
        captured_payload(&captured, 0),
        payload(),
        "the local copy carries the fan-out payload verbatim, inside the wire's framing"
    );
    assert_eq!(
        drain(&mut rx).len(),
        1,
        "the peer send is unaffected by the local copy"
    );
}

/// The async fan-out is a separate code path with the same obligation — the
/// sentinel's `send_to_nodes` and the executor's `send_to_finalizer` both drive
/// this one.
#[tokio::test]
async fn the_async_fanout_owes_the_local_copy_too() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let (sink, captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Finalizer], sink);

    reg.send_to_all(payload(), &NodeRegistryType::Finalizer).await;

    assert_eq!(captured.lock().unwrap().len(), 1);
    assert_eq!(captured_payload(&captured, 0), payload());
}

/// A lone composite — no peers registered at all — still has to be able to run
/// its own pipeline. The local copy is owed by *membership*, not by the bucket
/// having anyone in it.
#[test]
fn a_fanout_owes_the_local_copy_even_with_no_peers_registered() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let (sink, captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Finalizer], sink);

    reg.send_to_all_blocking(payload(), &NodeRegistryType::Finalizer);

    assert_eq!(
        captured.lock().unwrap().len(),
        1,
        "an empty bucket is not a reason to withhold the copy this host owes itself"
    );
}

/// The other half of "never double": a fan-out to a role this host does NOT run
/// is a pure peer message. A single-role committer must not start receiving
/// copies of its own gossip — that behaviour is what a split deployment relies
/// on, and it is the reason the seam is installed by the composite bridge and
/// not derived from the node's declared types.
#[test]
fn a_fanout_to_a_role_this_host_does_not_run_delivers_nothing_locally() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let (sink, captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Committer], sink);

    let (tx, mut rx) = mpsc::channel(16);
    assert!(
        reg.register_peer(
            vec![0xA1; 32],
            [1u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { tx }),
        ),
        "the remote finalizer must be admitted"
    );

    reg.send_to_all_blocking(payload(), &NodeRegistryType::Finalizer);

    assert!(
        captured.lock().unwrap().is_empty(),
        "this host does not run Finalizer, so it is not a member of the target set"
    );
    assert_eq!(drain(&mut rx).len(), 1);
    assert!(
        !reg.serves_locally(&NodeRegistryType::Finalizer),
        "the role-emptiness question a caller may ask must agree with the delivery decision"
    );
}

/// The seam cannot be switched on by configuration: a node's declared types are
/// what it *advertises* to peers, and every full-node config declares all four.
/// Only `install_self_delivery` — which the composite bridge calls with the
/// plugins it actually installed — makes a host a member of its own fan-out.
#[test]
fn declaring_a_role_does_not_subscribe_to_its_own_fanout() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    reg.set_declared_roles(vec![NodeRegistryType::Finalizer]);

    assert!(
        reg.self_delivery_roles().is_empty(),
        "nothing is subscribed until the bridge installs the subscription"
    );
    assert!(
        !reg.serves_locally(&NodeRegistryType::Finalizer),
        "a declared type is not a locally served one"
    );
}

/// Exactly-once, structural: if this host's own key *is* in the target bucket
/// (the composite e2e fixture convention registers it, and an operator could
/// configure it), the fan-out already reaches it and the local copy must be
/// suppressed rather than delivering the message twice.
#[test]
fn a_host_already_in_the_target_bucket_gets_no_second_copy() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let own_key = reg.get_config().public_key.clone();
    let (sink, captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Finalizer], sink);

    let (tx, mut rx) = mpsc::channel(16);
    assert!(
        reg.register_peer(
            own_key,
            [9u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { tx }),
        ),
        "the fixture registers the node's own identity as a peer"
    );

    reg.send_to_all_blocking(payload(), &NodeRegistryType::Finalizer);

    assert_eq!(drain(&mut rx).len(), 1, "the bucket entry is the delivery");
    assert!(
        captured.lock().unwrap().is_empty(),
        "a second copy would be a double delivery to the same member"
    );
}

/// A targeted send to this host's own key — the sentinel that selects its own
/// machine's finalizer, which on a composite is a quarter of transactions, not
/// an edge case. The bucket cannot resolve it, so before this seam it came back
/// as `undelivered` and the caller failed a transaction it could have served.
#[test]
fn a_targeted_send_to_our_own_key_is_delivered_locally() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let own_key = reg.get_config().public_key.clone();
    let (sink, captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Finalizer], sink);

    let undelivered = reg.send_to_peers_blocking(
        std::slice::from_ref(&own_key),
        &NodeRegistryType::Finalizer,
        &payload(),
    );

    assert!(
        undelivered.is_empty(),
        "a self-target this host serves is a delivery, not a miss: {undelivered:?}"
    );
    assert_eq!(captured.lock().unwrap().len(), 1);
    assert_eq!(captured_payload(&captured, 0), payload());
}

/// A targeted send is keyed, not typed: naming another node's key does not make
/// this host a member of the send, even when it runs the role. Otherwise a
/// shard preload for one committee would fan out to every local role.
#[test]
fn a_targeted_send_to_another_key_owes_no_local_copy() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let (sink, captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Finalizer], sink);

    let other = vec![0xB2; 32];
    let undelivered = reg.send_to_peers_blocking(
        std::slice::from_ref(&other),
        &NodeRegistryType::Finalizer,
        &payload(),
    );

    assert!(
        captured.lock().unwrap().is_empty(),
        "the send names a different node; this host is not its target"
    );
    assert_eq!(
        undelivered,
        vec![other],
        "and the unresolved remote target is still reported undelivered"
    );
}

/// Fail-closed on the targeted path: a self-target for a role this host does
/// not run must NOT be quietly "delivered" to a role that cannot serve it — it
/// stays an undelivered target, which is what makes `NoTarget` reach the caller.
#[test]
fn a_self_target_for_an_unserved_role_stays_undelivered() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let own_key = reg.get_config().public_key.clone();
    let (sink, captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Executor], sink);

    let undelivered = reg.send_to_peers_blocking(
        std::slice::from_ref(&own_key),
        &NodeRegistryType::Finalizer,
        &payload(),
    );

    assert_eq!(undelivered, vec![own_key], "no local Finalizer exists to take it");
    assert!(captured.lock().unwrap().is_empty());
}

/// Delivery is only half of it. Every inbound handler authenticates its sender
/// by asking this registry which role the key holds, and a same-host hop's
/// sender is this host's own identity — which peering never put in a bucket.
/// Without the self-subscription feeding role resolution, the copy a role owes
/// its own host arrives and is refused as an `Unregistered` sender: delivered,
/// then discarded, with the chain no closer to growing.
#[test]
fn a_hosts_own_key_holds_the_roles_it_runs() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let own_key = reg.get_config().public_key.clone();
    let (sink, _captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Finalizer], sink);

    assert_eq!(
        reg.find_node_types_by_public_key(&own_key),
        vec![NodeRegistryType::Finalizer],
        "the host's own key resolves to the role its bridge installed"
    );
    assert!(
        reg.node_may_send_action(&own_key, &[NodeRegistryType::Finalizer]),
        "so the Finalizer-gated action it fans out authenticates on arrival"
    );
    assert!(
        !reg.node_may_send_action(&own_key, &[NodeRegistryType::Committer]),
        "and the gate stays narrow: a role the host does not run is not held"
    );
}

/// The resolution is scoped to this host's key. Any other unregistered key must
/// stay `Unregistered` — otherwise the self-subscription would be a blanket
/// trust relaxation rather than a statement about one identity.
#[test]
fn role_resolution_for_any_other_key_is_unchanged() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let (sink, _captured) = recording_sink();
    reg.install_self_delivery(vec![NodeRegistryType::Finalizer], sink);

    let stranger = vec![0xCD; 32];
    assert!(
        reg.find_node_types_by_public_key(&stranger).is_empty(),
        "a stranger is still an unregistered sender"
    );
    assert!(!reg.node_may_send_action(&stranger, &[NodeRegistryType::Finalizer]));
}

/// And without a self-subscription — every single-role process — the host's own
/// key resolves to no roles at all, exactly as it did before this seam existed.
#[test]
fn without_a_self_subscription_the_own_key_holds_no_roles() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 10)]);
    let own_key = reg.get_config().public_key.clone();

    assert!(
        reg.find_node_types_by_public_key(&own_key).is_empty(),
        "a split-deployment host gains no self-role resolution from declaring types"
    );
}
