//! Tests for the control-plane **client** half (`peering.rs`) and for what a
//! peer's handler does with what it sends — including the `RegisterAck`
//! placement rule, which is the difference between a cluster that routes and
//! one where every node files its peers under the wrong roles.
//!
//! Everything here runs with `network = None` (registry test mode) by passing
//! the built requests between two registries by hand. That is deliberate: the
//! interesting properties are about signatures, role sets and buckets, all of
//! which are decided before any socket is touched — and the framing that does
//! need the transport is pinned separately, both by size against the
//! direct-packet cap and by the two frame-shape tests below.
//!
//! Directory admission also asks whether a responder can *reach* the asker, which
//! is transport state. With `network = None` there is no route table to consult,
//! so the reachability answer is seeded via `seed_route_for_test` — which fails
//! closed by default, so these tests start from "cannot reach" exactly as an
//! unseeded production registry would, and the live transport tests in
//! `tests/peered_topology_e2e.rs` cover the announce path that fills it.
use super::helpers::*;
use super::super::*;
use crate::config::BootstrapPeer;
use crate::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use crate::node::NetworkPacket;
use crate::rns::identity::rhash_from_public_key;
use crate::rns::wrapper::DIRECT_PACKET_PLAINTEXT_MAX;

const ALL_TYPES: &[(NodeRegistryType, usize)] = &[
    (NodeRegistryType::Finalizer, 5),
    (NodeRegistryType::Executor, 5),
    (NodeRegistryType::Sentinel, 5),
    (NodeRegistryType::Committer, 5),
];

fn key_of(identity: &std::sync::Arc<crate::rns::identity::NodeIdentity>) -> Vec<u8> {
    identity.ed25519.public_key().expect("ed25519 public key")
}

/// A `Register` carries exactly the roles this node serves, and the peer puts
/// it in exactly those buckets — no more. The "no more" is the point: a node
/// that appears in a bucket it cannot serve receives traffic it will fail.
#[test]
fn register_lands_in_declared_buckets_only() {
    let alice = test_identity();
    let bob = test_identity();
    let alice_reg = registry_for(
        &alice,
        ALL_TYPES,
        &[NodeRegistryType::Executor, NodeRegistryType::Sentinel],
        vec![bootstrap_peer_for(&bob, 4242)],
    );
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    let request = alice_reg.build_register_request().expect("build register");
    assert_eq!(
        request.requester_types,
        vec![NodeRegistryType::Executor, NodeRegistryType::Sentinel]
    );
    // Highest-priority declared role, matching the ack the responder will send.
    assert_eq!(request.requested_type, NodeRegistryType::Executor);

    bob_reg
        .handle_control(request)
        .expect("register handled");

    let alice_key = key_of(&alice);
    assert_eq!(bucket_len(&bob_reg, &NodeRegistryType::Executor), 1);
    assert_eq!(bucket_len(&bob_reg, &NodeRegistryType::Sentinel), 1);
    assert_eq!(bucket_len(&bob_reg, &NodeRegistryType::Finalizer), 0);
    assert_eq!(bucket_len(&bob_reg, &NodeRegistryType::Committer), 0);
    // The multi-bucket lookup, in registry order — not priority order, which is
    // what `find_node_type_by_public_key` (singular) reports.
    assert_eq!(
        bob_reg.find_node_types_by_public_key(&alice_key),
        vec![NodeRegistryType::Sentinel, NodeRegistryType::Executor]
    );
}

/// The binding covers the role set itself, so a `Register` whose declared set
/// was widened in transit no longer verifies and admits nothing. Without this
/// a node would trust a claim about its roles that its key never signed.
#[test]
fn widening_the_declared_role_set_breaks_the_binding() {
    let alice = test_identity();
    let bob = test_identity();
    let alice_reg = registry_for(
        &alice,
        ALL_TYPES,
        &[NodeRegistryType::Sentinel],
        vec![bootstrap_peer_for(&bob, 4242)],
    );
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    let mut request = alice_reg.build_register_request().expect("build register");
    request.requester_types.push(NodeRegistryType::Finalizer);

    bob_reg
        .handle_control(request)
        .expect("handled, and rejected inside the handler");

    for node_type in NodeRegistry::routed_role_types() {
        assert_eq!(
            bucket_len(&bob_reg, &node_type),
            0,
            "a tampered role set must admit nothing"
        );
    }
}

/// A node that declares no roles is not a node that wants to join the network.
/// Building a request anyway would only produce a guaranteed rejection and
/// mislabel a caller bug as a peering failure.
#[test]
fn an_empty_declared_role_set_fails_closed() {
    let alice = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[], vec![]);
    assert!(alice_reg.build_register_request().is_err());
    assert!(alice_reg.build_heartbeat_request().is_err());
    // Nothing to send, and no panic: the sender reports zero rather than
    // producing a request it knows will be refused.
    assert_eq!(alice_reg.register_with_bootstrap_peers(), 0);
}

/// THE placement rule. `node_type` in a `RegisterAck` names the type *the
/// requester* was registered under; it says nothing about the responder. A
/// finalizer that files a committer under `Finalizer` because *it* is a
/// finalizer then sends that committer `Sign` traffic it cannot serve, and
/// finds nobody in the buckets it does route to.
#[test]
fn an_ack_files_the_responder_under_the_roles_it_declared() {
    let alice = test_identity();
    let bob = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Finalizer], vec![]);
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    // Alice (a finalizer) registers with Bob (a committer).
    let request = alice_reg.build_register_request().expect("build register");
    bob_reg
        .handle_control(request.clone())
        .expect("register handled");
    // Bob's ack therefore names Finalizer — Alice's type, not Bob's.
    let ack = bob_reg
        .build_register_ack(true, NodeRegistryType::Finalizer, "")
        .expect("build ack");
    assert_eq!(ack.requested_type, NodeRegistryType::Finalizer);
    assert_eq!(ack.requester_types, vec![NodeRegistryType::Committer]);

    alice_reg
        .handle_control(ack)
        .expect("ack handled");

    assert_eq!(
        bucket_len(&alice_reg, &NodeRegistryType::Committer),
        1,
        "Bob is a committer, so he belongs in Alice's committer bucket"
    );
    assert_eq!(
        bucket_len(&alice_reg, &NodeRegistryType::Finalizer),
        0,
        "the acked type describes Alice, not Bob"
    );
    assert_eq!(
        alice_reg.find_node_type_by_public_key(&key_of(&bob)),
        Some(NodeRegistryType::Committer)
    );
}

/// A peer that declares nothing for itself (no compliant responder does;
/// `build_register_ack` always fills the set) is still filed *somewhere*
/// rather than dropped — the acked type, which is what the old behaviour did
/// unconditionally.
#[test]
fn an_ack_declaring_nothing_falls_back_to_the_acked_type() {
    let alice = test_identity();
    let bob = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Finalizer], vec![]);

    let node_type = NodeRegistryType::Committer;
    let binding = bob
        .sign_binding(&bob.rhash, &node_type, &[])
        .expect("sign degenerate ack");
    let ack = NodeRequest {
        requester_key: key_of(&bob),
        requester_rhash: bob.rhash,
        request_type: NodeRequestType::RegisterAck {
            accepted: true,
            node_type: node_type.clone(),
            responder_key: key_of(&bob),
            reason: String::new(),
        },
        requester_types: vec![],
        requested_type: node_type,
        binding_signature: binding,
        query_target: None,
        query_nonce: None,
    };

    alice_reg
        .handle_control(ack)
        .expect("ack handled");
    assert_eq!(bucket_len(&alice_reg, &NodeRegistryType::Committer), 1);
}

/// A peer learned from an ack carries that ack's binding, so *we* can vouch
/// for it in our own directory responses — `handle_request` omits entries with
/// an empty `directory_signature`. This is what lets a cluster grow past its
/// bootstrap lists; the receiver of a directory re-verifies the entry against
/// the listed node's own key, which is the check asserted here.
#[test]
fn an_ack_learned_peer_is_vouchable_in_our_directory() {
    let alice = test_identity();
    let bob = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    let request = alice_reg.build_register_request().expect("build register");
    bob_reg.handle_control(request).expect("register handled");
    let ack = bob_reg
        .build_register_ack(true, NodeRegistryType::Sentinel, "")
        .expect("build ack");
    alice_reg.handle_control(ack).expect("ack handled");

    // Read the stored binding out field by field: `NodeRegistryNode` holds a
    // `Box<dyn Connection>`, so it is not `Clone`.
    let (signature, requested_type, node_types) = {
        let nodes = alice_reg
            .get_nodes(&NodeRegistryType::Committer)
            .expect("committer bucket");
        let entry = nodes.get(&key_of(&bob)).expect("Bob installed");
        let value = entry.value();
        (
            value.directory_signature.clone(),
            value.directory_requested_type.clone(),
            value.directory_node_types.clone(),
        )
    };

    assert!(
        !signature.is_empty(),
        "an ack-learned peer must be vouchable, or it can never appear in a directory"
    );
    assert!(
        crate::rns::identity::NodeIdentity::verify_binding(
            &key_of(&bob),
            &bob.rhash,
            &requested_type,
            &node_types,
            &signature,
        ),
        "the stored binding must re-verify exactly as a directory receiver will"
    );
}
/// Directory-learned peers were never registered here, so a heartbeat is the
/// only thing keeping them alive. The built request must actually refresh one.
#[test]
fn a_heartbeat_refreshes_a_known_peer() {
    let alice = test_identity();
    let bob = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    let request = alice_reg.build_register_request().expect("build register");
    bob_reg.handle_control(request).expect("register handled");
    let ack = bob_reg
        .build_register_ack(true, NodeRegistryType::Sentinel, "")
        .expect("build ack");
    alice_reg.handle_control(ack).expect("ack handled");

    // Age *Alice's entry in Bob's registry* out by hand, the way 30 s of
    // silence would: a heartbeat refreshes the sender's entry in the
    // receiver's directory, which is the peer that would otherwise evict her.
    let alice_key = key_of(&alice);
    {
        let nodes = bob_reg
            .get_nodes(&NodeRegistryType::Sentinel)
            .expect("sentinel bucket");
        let mut entry = nodes.get_mut(&alice_key).expect("Alice registered");
        entry.value_mut().last_seen =
            std::time::Instant::now() - std::time::Duration::from_secs(120);
    }

    let heartbeat = alice_reg.build_heartbeat_request().expect("build heartbeat");
    bob_reg.handle_control(heartbeat).expect("heartbeat handled");

    let refreshed = bob_reg
        .get_nodes(&NodeRegistryType::Sentinel)
        .expect("sentinel bucket")
        .get(&alice_key)
        .expect("Alice still registered")
        .value()
        .last_seen;
    assert!(
        refreshed.elapsed() < std::time::Duration::from_secs(5),
        "a verified heartbeat must refresh last_seen, or directories evict live peers"
    );
}

/// A composite registered under four roles sits in four maps. Heartbeats are
/// per peer, not per bucket, or every tick would send it four.
#[test]
fn known_peers_are_deduplicated_across_buckets() {
    let alice = test_identity();
    let bob = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    let request = alice_reg.build_register_request().expect("build register");
    bob_reg.handle_control(request).expect("register handled");
    // Ack declaring all four roles: Bob now sits in four of Alice's buckets.
    let binding = bob
        .sign_binding(
            &bob.rhash,
            &NodeRegistryType::Finalizer,
            &[
                NodeRegistryType::Finalizer,
                NodeRegistryType::Executor,
                NodeRegistryType::Sentinel,
                NodeRegistryType::Committer,
            ],
        )
        .expect("sign multi-role ack");
    let ack = NodeRequest {
        requester_key: key_of(&bob),
        requester_rhash: bob.rhash,
        request_type: NodeRequestType::RegisterAck {
            accepted: true,
            node_type: NodeRegistryType::Finalizer,
            responder_key: key_of(&bob),
            reason: String::new(),
        },
        requester_types: vec![
            NodeRegistryType::Finalizer,
            NodeRegistryType::Executor,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Committer,
        ],
        requested_type: NodeRegistryType::Finalizer,
        binding_signature: binding,
        query_target: None,
        query_nonce: None,
    };
    alice_reg.handle_control(ack).expect("ack handled");

    assert_eq!(bucket_len(&alice_reg, &NodeRegistryType::Finalizer), 1);
    assert_eq!(bucket_len(&alice_reg, &NodeRegistryType::Committer), 1);
    assert_eq!(
        alice_reg.known_peers().len(),
        1,
        "one peer in four buckets is one peer"
    );
}

/// The frame shape that used to be sent: a bare `NodeRequest`, not wrapped in a
/// `NetworkPacket`. It is not merely ignored — it *decodes successfully* as a
/// `NetworkPacket` with both fields `None` (rmp named maps ignore unknown keys,
/// and serde reads an absent `Option` field as `None`), so the receiver drops
/// it with no error and no log line. This is why `send_control` exists.
#[test]
fn bare_node_request_bytes_are_a_black_hole() {
    let alice = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);
    let request = alice_reg.build_register_request().expect("build register");

    let bare = serialize_to_bytes_rmp(&request).expect("serialize bare request");
    match deserialize_rmp_to::<NetworkPacket>(&bare) {
        Ok(packet) => {
            assert!(
                packet.control.is_none() && packet.data.is_none(),
                "a bare request must carry neither plane — that is the silent drop"
            );
        }
        // Also a black hole: a receiver that cannot decode it logs and drops.
        Err(_) => {}
    }
}

/// The frame shape that works: the same request under `NetworkPacket::control`
/// decodes with the control plane populated, which is the exact dispatch path
/// both binaries use (`handle_control`).
#[test]
fn a_control_frame_reaches_the_peers_control_plane() {
    let alice = test_identity();
    let bob = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    let request = alice_reg.build_register_request().expect("build register");
    let framed = serialize_to_bytes_rmp(&NetworkPacket {
        control: Some(request),
        data: None,
    })
    .expect("serialize control frame");

    let packet: NetworkPacket = deserialize_rmp_to(&framed).expect("frame decodes");
    let control = packet.control.expect("control plane present");
    bob_reg.handle_control(control).expect("handled");
    assert_eq!(bucket_len(&bob_reg, &NodeRegistryType::Committer), 1);
}

/// A control frame does **not** fit the direct-packet cap, and that is the
/// boot-ordering constraint for peering. The binding signature is the hybrid
/// `[Ed25519 sig · ML-DSA pk · ML-DSA sig]` (~3.8 KB), which puts a `Register`
/// at roughly twelve times the ~481 B a direct RNS packet can carry — so a
/// control frame can only ride the Resource path, which needs a *live* route,
/// and a route seeded from `bootstrap_peers` is not one until the peer's
/// announce arrives.
///
/// Consequence, pinned here so it cannot be "optimised" away: **a node cannot
/// peer at t=0.** It peers after it has heard an announce from the peer, which
/// is why `send_control` reports "no live route yet" instead of blocking, and
/// why `start_peering` is a retry loop rather than a boot-time burst. A cluster
/// whose nodes never announce to each other can never register, no matter how
/// the control messages are framed.
#[test]
fn control_frames_exceed_the_direct_cap_so_peering_must_wait_for_a_route() {
    let alice = test_identity();
    let bob = test_identity();
    // Worst case: every role declared, and a bootstrap peer configured.
    let reg = registry_for(
        &alice,
        ALL_TYPES,
        &[
            NodeRegistryType::Finalizer,
            NodeRegistryType::Executor,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Committer,
        ],
        vec![bootstrap_peer_for(&bob, 4242)],
    );
    let peer_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    let messages = {
        let mut out = vec![
            ("Register", reg.build_register_request().expect("register")),
            ("Heartbeat", reg.build_heartbeat_request().expect("heartbeat")),
        ];
        for node_type in NodeRegistry::routed_role_types() {
            out.push((
                "Request",
                reg.build_directory_request(&node_type, bob.rhash)
                    .expect("directory request"),
            ));
        }
        out.push((
            "RegisterAck",
            peer_reg
                .build_register_ack(true, NodeRegistryType::Finalizer, "")
                .expect("ack"),
        ));
        out
    };

    let mut all_over_cap = true;
    for (name, request) in messages {
        let bytes = serialize_to_bytes_rmp(&NetworkPacket {
            control: Some(request),
            data: None,
        })
        .expect("serialize");
        eprintln!(
            "{name} control frame: {} B (direct cap {} B)",
            bytes.len(),
            DIRECT_PACKET_PLAINTEXT_MAX
        );
        all_over_cap &= bytes.len() > DIRECT_PACKET_PLAINTEXT_MAX;
    }
    assert!(
        all_over_cap,
        "every control frame is expected to exceed the direct-packet cap; if one now \
         fits, it can be sent before an announce and this test's premise needs revisiting"
    );
}

/// A bootstrap peer is addressed by the hex **RNS** public key (the same field
/// the transport pre-seeds routes from), so the rhash must derive identically.
/// A malformed entry is skipped rather than poisoning the whole list.
#[test]
fn bootstrap_peers_are_resolved_to_transport_addresses() {
    let alice = test_identity();
    let bob = test_identity();
    let malformed = BootstrapPeer {
        public_key: "deadbeef".to_string(),
        ip: "127.0.0.1".to_string(),
        port: 4243,
    };
    let reg = registry_for(
        &alice,
        ALL_TYPES,
        &[NodeRegistryType::Committer],
        vec![bootstrap_peer_for(&bob, 4242), malformed],
    );

    let peers = reg.bootstrap_peer_rhashes();
    assert_eq!(peers.len(), 1, "the malformed key is skipped, not fatal");
    let bob_bytes: [u8; 64] = bob
        .rns
        .get_public_key()
        .expect("rns public key")
        .try_into()
        .expect("64-byte rns key");
    assert_eq!(
        peers[0].0,
        rhash_from_public_key(&bob_bytes),
        "the peer rhash must match what the transport derives"
    );
    assert_eq!(peers[0].0, bob.rhash, "and what the peer's own config claims");
}

/// Test mode has no transport, and a peering call must say so rather than
/// silently "succeed" while delivering nothing — the failure mode that made
/// this whole gap invisible in the first place.
#[test]
fn peering_without_a_transport_reports_failure_instead_of_lying() {
    let alice = test_identity();
    let reg = registry_for(
        &alice,
        ALL_TYPES,
        &[NodeRegistryType::Committer],
        vec![bootstrap_peer_for(&test_identity(), 4242)],
    );

    let request = reg.build_register_request().expect("build register");
    let err = reg
        .send_control([9u8; 16], request)
        .expect_err("no transport, so this cannot have been sent");
    assert!(
        matches!(err, crate::errors::PneumaticError::Registry(_)),
        "expected a Registry error, got {err:?}"
    );
    assert_eq!(reg.register_with_bootstrap_peers(), 0);
    assert_eq!(reg.request_directories_from_bootstrap_peers(), 0);

    let shared = std::sync::Arc::new(reg);
    assert!(
        !shared.start_peering(),
        "no transport means no peering loop, and it must say so"
    );
}

/// `declared_roles` is the single source every outbound message describes this
/// node with; `set_declared_roles` is how a composite narrows it to the plugins
/// it actually installed.
#[test]
fn declared_roles_drive_every_outbound_message() {
    let alice = test_identity();
    let reg = registry_for(
        &alice,
        ALL_TYPES,
        &[
            NodeRegistryType::Finalizer,
            NodeRegistryType::Committer,
        ],
        vec![],
    );
    assert_eq!(
        reg.declared_roles(),
        vec![NodeRegistryType::Finalizer, NodeRegistryType::Committer]
    );
    assert_eq!(
        reg.build_register_request().expect("register").requested_type,
        NodeRegistryType::Finalizer,
        "highest priority wins"
    );

    reg.set_declared_roles(vec![NodeRegistryType::Committer]);
    let request = reg.build_register_request().expect("register");
    assert_eq!(request.requester_types, vec![NodeRegistryType::Committer]);
    assert_eq!(request.requested_type, NodeRegistryType::Committer);
    assert!(
        NodeRegistry::primary_registry_type(&request.requester_types)
            == Some(NodeRegistryType::Committer)
    );
}

/// The directory is a validator-set listing, not a public bulletin board.
/// Before the peering initiator nothing sent directory requests, so the handler
/// had never run on a live path and answered any caller.
#[test]
fn a_directory_request_from_a_stranger_is_not_answered() {
    let alice = test_identity();
    let bob = test_identity();
    let stranger = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);

    // A well-formed request from a node that never registered here and has never
    // been in contact with us. It carries a valid signature and a fresh nonce, so
    // nothing about it is malformed — it is refused because a stranger firing one
    // cold packet has not earned the validator-set listing.
    let request = {
        let stranger_reg = registry_for(&stranger, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
        stranger_reg
            .build_directory_request(&NodeRegistryType::Committer, alice.rhash)
            .expect("build request")
    };
    assert!(
        alice_reg.build_directory_response(&request).is_none(),
        "an unregistered, unreachable requester must not receive the directory"
    );

    // And the specific shape of the old directory *response*, which carried a
    // `Request` echo with an empty binding: even if a peer echoes one back, it
    // is not answerable, so the reply loop the echo used to cause cannot start.
    let echo = NodeRequest {
        requester_key: bob.rhash.to_vec(),
        requester_rhash: bob.rhash,
        request_type: NodeRequestType::Request,
        requester_types: vec![],
        requested_type: NodeRegistryType::Committer,
        binding_signature: vec![],
        query_target: None,
        query_nonce: None,
    };
    assert!(alice_reg.build_directory_response(&echo).is_none());
}

/// The other side of the gate: a peer that registered *is* answered, and the
/// listing carries only nodes this node can vouch for.
#[test]
fn a_registered_peer_is_answered_with_the_peers_it_can_vouch_for() {
    let alice = test_identity();
    let bob = test_identity();
    let carol = test_identity();
    let alice_reg = registry_for(
        &alice,
        ALL_TYPES,
        &[NodeRegistryType::Sentinel],
        vec![bootstrap_peer_for(&bob, 4242)],
    );
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    // Bob registers with Alice.
    let register = bob_reg.build_register_request().expect("build register");
    alice_reg.handle_control(register).expect("register handled");

    // A third node sitting in Alice's committer bucket *without* a binding
    // (learned from a directory, or seeded) must never be listed.
    alice_reg.register_peer(
        key_of(&carol),
        carol.rhash,
        &NodeRegistryType::Committer,
        Box::new(NullConnection),
    );

    let request = bob_reg
        .build_directory_request(&NodeRegistryType::Committer, alice.rhash)
        .expect("build request");
    let response = alice_reg
        .build_directory_response(&request)
        .expect("a registered peer is answered");

    assert_eq!(
        response.entries.len(),
        1,
        "only the node Alice holds a binding for is listable"
    );
    assert_eq!(response.entries[0].node_key, key_of(&bob));
    assert_eq!(response.registry_type, NodeRegistryType::Committer);
    assert_eq!(response.responder_key, key_of(&alice));

    // The entry is Bob's own binding, which is what makes the listing
    // trustworthy without trusting Alice: re-verify it against Bob's key.
    assert!(
        crate::rns::identity::NodeIdentity::verify_binding(
            &response.entries[0].node_key,
            &response.entries[0].node_rhash,
            &response.entries[0].requested_type,
            &response.entries[0].node_types,
            &response.entries[0].signature,
        ),
        "a directory entry must verify against the listed node's own key"
    );
}

// ---------------------------------------------------------------------------
// Directory admission: freshness, replay, and the unregistered observer
// ---------------------------------------------------------------------------/// A node is not its own peer. Registration is pairwise, so our entry rides
/// every peer's buckets; any directory response we ever fetch therefore
/// names us right back at us. Installing it would plant our own rhash in
/// our own directories — the exact state mesh-probe flags as `self-present`,
/// observed on all four nodes of the 10/08/2026 multi-host rehearsal. The
/// receiver must skip entries that name itself.
#[test]
fn a_directory_response_naming_ourselves_never_lands_in_our_own_directories() {
    let alice = test_identity();
    let bob = test_identity();
    let carol = test_identity();

    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);
    let carol_reg = registry_for(&carol, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);

    // Alice holds Bob and Carol as vouched committers.
    alice_reg
        .handle_control(bob_reg.build_register_request().expect("bob register"))
        .expect("bob handled");
    alice_reg
        .handle_control(carol_reg.build_register_request().expect("carol register"))
        .expect("carol handled");

    // Bob asks Alice for the committers directory.
    let request = bob_reg
        .build_directory_request(&NodeRegistryType::Committer, alice.rhash)
        .expect("build request");
    let response = alice_reg
        .build_directory_response(&request)
        .expect("a registered peer is answered");
    assert!(
        response.entries.iter().any(|e| e.node_rhash == bob.rhash),
        "the directory necessarily names Bob back at himself"
    );

    // Bob must hold Alice (registered or seeded) for the response to be trusted.
    bob_reg.register_peer(
        key_of(&alice),
        alice.rhash,
        &NodeRegistryType::Committer,
        Box::new(NullConnection),
    );
    bob_reg
        .handle_directory_response(&response)
        .expect("response from a known peer is accepted");

    let held: Vec<[u8; 16]> = bob_reg
        .get_nodes(&NodeRegistryType::Committer)
        .unwrap()
        .iter()
        .map(|e| e.value().rhash)
        .collect();
    assert!(held.contains(&carol.rhash), "the third-party peer is installed");
    assert!(
        !held.contains(&bob.rhash),
        "a directory response must never install the reader's own rhash"
    );
}

/// Directory pulls must be retryable — and must stop once complete. The
/// first-round-only fetch raced the announces: a node that boots while its
/// peers' routes are not yet live gets its requests refused, and its peers'
/// Register acks carry only the acking peer's own entry, so the late node is
/// left permanently directory-less while its routes heal underneath
/// (rehearsal, 10/08/2026: a restarted container held live routes to all
/// three peers and `pneumatic_node_peers 0` forever). The catch-up asks
/// exactly the bootstrap peers not yet learned — no more while they are
/// missing, and, crucially, no more once they are held, so a healthy steady
/// state sends zero directory traffic.
#[test]
fn catch_up_asks_exactly_the_bootstrap_peers_not_yet_learned() {
    let alice = test_identity();
    let bob = test_identity();
    let carol = test_identity();
    let alice_reg = registry_for(
        &alice,
        ALL_TYPES,
        &[NodeRegistryType::Sentinel],
        vec![bootstrap_peer_for(&bob, 4242), bootstrap_peer_for(&carol, 4243)],
    );
    // The rhashes the bootstrap list resolves to — the same derivation the
    // registration path stores, which is what makes "held" comparable.
    let bootstraps = alice_reg.bootstrap_peer_rhashes();
    assert_eq!(bootstraps.len(), 2, "both bootstrap peers resolve");

    // At boot nothing is held: catch-up targets every bootstrap peer.
    let needing: std::collections::HashSet<[u8; 16]> = alice_reg
        .bootstrap_peers_needing_directory()
        .into_iter()
        .map(|(rhash, _desc)| rhash)
        .collect();
    let wanted: std::collections::HashSet<[u8; 16]> =
        bootstraps.iter().map(|(rhash, _desc)| *rhash).collect();
    assert_eq!(needing, wanted, "at boot every bootstrap peer needs the directory");

    // Learning Bob — in any one bucket — takes him off the list; Carol stays.
    alice_reg.register_peer(
        key_of(&bob),
        bootstraps[0].0,
        &NodeRegistryType::Sentinel,
        Box::new(NullConnection),
    );
    let needing: std::collections::HashSet<[u8; 16]> = alice_reg
        .bootstrap_peers_needing_directory()
        .into_iter()
        .map(|(rhash, _desc)| rhash)
        .collect();
    assert_eq!(
        needing,
        [bootstraps[1].0].into_iter().collect::<std::collections::HashSet<_>>(),
        "a learned peer must drop off the re-request list"
    );

    // Learning Carol leaves nothing to ask: a healthy steady state sends no
    // directory traffic at all (this is what keeps the retry from becoming
    // a permanent gossip burden).
    alice_reg.register_peer(
        key_of(&carol),
        bootstraps[1].0,
        &NodeRegistryType::Sentinel,
        Box::new(NullConnection),
    );
    assert!(
        alice_reg.bootstrap_peers_needing_directory().is_empty(),
        "once every bootstrap peer is held, catch-up must go silent"
    );
}

/// Build a fresh query from `asker` to `responder`, asking for `for_type`.
fn query_from(
    asker: &NodeRegistry,
    for_type: &NodeRegistryType,
    responder_rhash: [u8; 16],
) -> NodeRequest {
    asker
        .build_directory_request(for_type, responder_rhash)
        .expect("build directory query")
}

/// The reason the directory gate stopped being a stake gate. A monitor, explorer,
/// or load generator that holds no stake is not in the validator set and does not
/// inflate the quorum denominator — but it still needs to learn who the validators
/// are. It can, by proving it is in contact with us and signing a query addressed
/// to us alone. No registration, no stake, no slot in the denominator.
#[test]
fn an_observer_that_never_registered_is_answered_once_it_is_in_contact() {
    let alice = test_identity();
    let listed = test_identity();
    let observer = test_identity();

    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
    // Someone Alice registered, so the answer has real content to withhold.
    let listed_reg = registry_for(
        &listed,
        ALL_TYPES,
        &[NodeRegistryType::Committer],
        vec![],
    );
    alice_reg
        .handle_control(listed_reg.build_register_request().expect("register"))
        .expect("register handled");

    let observer_reg = registry_for(
        &observer,
        ALL_TYPES,
        &[NodeRegistryType::Archiver],
        vec![],
    );

    // Cold: perfectly well-formed, correctly signed, never been in contact.
    let cold = query_from(&observer_reg, &NodeRegistryType::Committer, alice.rhash);
    assert!(
        alice_reg.build_directory_response(&cold).is_none(),
        "a query from a peer we cannot reach must not be answered"
    );

    // In contact — the announce path, which these tests stand in for by seeding.
    alice_reg.seed_route_for_test(observer.rhash);
    let warm = query_from(&observer_reg, &NodeRegistryType::Committer, alice.rhash);
    let answer = alice_reg
        .build_directory_response(&warm)
        .expect("an observer in contact is answered");
    assert_eq!(answer.entries.len(), 1);
    assert_eq!(answer.entries[0].node_key, key_of(&listed));

    // And the answer is Alice's own envelope over exactly (entries, type, her
    // rhash) — the observer verifies it without trusting anyone else.
    assert!(NodeIdentity::verify_message(
        &key_of(&alice),
        &directory_response_signature_payload(
            &answer.entries,
            &answer.registry_type,
            &alice.rhash
        )
        .expect("payload"),
        &answer.signature
    ));
}

/// The nonce is the whole difference between a query and a captured query. The
/// classic binding covers no responder and no freshness, so one observation of a
/// legitimate query was, until now, a subscription to the validator set.
#[test]
fn a_replayed_directory_query_is_refused() {
    let alice = test_identity();
    let observer = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
    alice_reg.seed_route_for_test(observer.rhash);
    let observer_reg = registry_for(&observer, ALL_TYPES, &[NodeRegistryType::Archiver], vec![]);

    let query = query_from(&observer_reg, &NodeRegistryType::Committer, alice.rhash);
    assert!(alice_reg.build_directory_response(&query).is_some());
    assert!(
        alice_reg.build_directory_response(&query).is_none(),
        "the same query resent must not be answered twice"
    );
    // A retry with a *new* nonce is unaffected — this is replay protection, not a
    // rate limit that punishes honest retries.
    let retry = query_from(&observer_reg, &NodeRegistryType::Committer, alice.rhash);
    assert!(alice_reg.build_directory_response(&retry).is_some());
}

/// A captured query names its responder. Pointing the same signature at a
/// different validator fails, so one peer cannot be used as a stamp to query
/// everyone a requester's key is known to.
#[test]
fn a_query_addressed_to_another_responder_is_refused() {
    let alice = test_identity();
    let carol = test_identity();
    let observer = test_identity();

    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
    alice_reg.seed_route_for_test(observer.rhash);
    let observer_reg = registry_for(&observer, ALL_TYPES, &[NodeRegistryType::Archiver], vec![]);

    let for_carol = query_from(&observer_reg, &NodeRegistryType::Committer, carol.rhash);
    assert!(
        alice_reg.build_directory_response(&for_carol).is_none(),
        "a query addressed to someone else is not ours to answer"
    );

    // Same key, same role set, addressed to us: answered. The refusal above was
    // about the target and nothing else.
    let for_alice = query_from(&observer_reg, &NodeRegistryType::Committer, alice.rhash);
    assert!(alice_reg.build_directory_response(&for_alice).is_some());
}

/// The signature covers the requested type and the declared role set, so editing
/// either after signing breaks it — a requester cannot ask for one bucket with a
/// signature obtained for another.
#[test]
fn a_query_tampered_after_signing_is_refused() {
    let alice = test_identity();
    let observer = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
    alice_reg.seed_route_for_test(observer.rhash);
    let observer_reg = registry_for(&observer, ALL_TYPES, &[NodeRegistryType::Archiver], vec![]);

    let mut inflated = query_from(&observer_reg, &NodeRegistryType::Committer, alice.rhash);
    inflated.requester_types = vec![
        NodeRegistryType::Archiver,
        NodeRegistryType::Committer,
        NodeRegistryType::Finalizer,
    ];
    assert!(
        alice_reg.build_directory_response(&inflated).is_none(),
        "claiming extra roles after signing must invalidate the query"
    );

    let mut retargeted = query_from(&observer_reg, &NodeRegistryType::Committer, alice.rhash);
    retargeted.requested_type = NodeRegistryType::Executor;
    assert!(
        alice_reg.build_directory_response(&retargeted).is_none(),
        "asking for a different bucket than was signed must fail"
    );
}

/// Domain separation, both directions. Adding a query signature to the control
/// plane is only safe if it cannot be spent as a registration binding — that
/// would hand the stake gate an override — and a registration binding must not be
/// spendable as a query.
#[test]
fn a_query_signature_cannot_register_and_a_registration_binding_cannot_query() {
    let alice = test_identity();
    let attacker = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
    let attacker_reg = registry_for(
        &attacker,
        ALL_TYPES,
        &[NodeRegistryType::Committer],
        vec![],
    );

    // Query signature → Register. The attacker holds no stake; if this were
    // admitted, the fresh-query path would be a door around the stake gate.
    let query = query_from(&attacker_reg, &NodeRegistryType::Committer, alice.rhash);
    let smuggled = NodeRequest {
        request_type: NodeRequestType::Register,
        binding_signature: query.binding_signature.clone(),
        query_target: None,
        query_nonce: None,
        ..query.clone()
    };
    alice_reg
        .handle_control(smuggled)
        .expect("register handled");
    assert_eq!(
        bucket_len(&alice_reg, &NodeRegistryType::Committer),
        0,
        "a query signature must not register anyone"
    );

    // Register binding → query. The attacker's classic binding, replayed as a
    // legacy query, still needs to be registered here — which it is not.
    let register = attacker_reg.build_register_request().expect("register");
    let as_query = NodeRequest {
        request_type: NodeRequestType::Request,
        requested_type: NodeRegistryType::Committer,
        query_target: None,
        query_nonce: None,
        ..register.clone()
    };
    assert!(
        alice_reg.build_directory_response(&as_query).is_none(),
        "a registration binding is not a query, and its owner is not registered"
    );
}

/// The compatibility path: a peer on a build that predates fresh queries is still
/// answered if it registered. Weaker — its binding covers neither responder nor
/// nonce — and honored only for that reason.
#[test]
fn a_legacy_query_from_a_registered_peer_is_still_answered() {
    let alice = test_identity();
    let bob = test_identity();
    let alice_reg = registry_for(&alice, ALL_TYPES, &[NodeRegistryType::Sentinel], vec![]);
    let bob_reg = registry_for(&bob, ALL_TYPES, &[NodeRegistryType::Committer], vec![]);
    alice_reg
        .handle_control(bob_reg.build_register_request().expect("register"))
        .expect("register handled");

    // A pre-freshness query: classic binding, no target, no nonce. (A heartbeat
    // already carries exactly that shape, so relabel one rather than hand-roll a
    // binding the client no longer produces.)
    let legacy = NodeRequest {
        request_type: NodeRequestType::Request,
        requested_type: NodeRegistryType::Committer,
        query_target: None,
        query_nonce: None,
        ..bob_reg.build_heartbeat_request().expect("heartbeat")
    };
    assert!(
        alice_reg.build_directory_response(&legacy).is_some(),
        "a registered peer on an older build must not be cut off mid-upgrade"
    );
}
