//! The control plane over real UDP: two RNS nodes bootstrap to each other,
//! run the peering loop, and end up in each other's role directories.
//!
//! Everything in `src/node/registry/tests/peering.rs` runs with the transport
//! stubbed out and the requests passed between registries by hand. That is what
//! makes those tests fast and deterministic, and it is also what they cannot
//! check: that a control frame survives RNS framing, fragmentation and the
//! Resource transfer a ~5.9 KB `Register` needs; that a bootstrap peer's
//! configured hex key resolves to the same transport address the transport
//! itself uses; that the ack comes back and lands in the right bucket.
//!
//! Those are the properties of this test. It is the first time in this repo's
//! history that two node registries exchange `Register`/`RegisterAck` over a
//! socket — until the peering initiator existed, no production code path sent a
//! control message at all, and the receiving half had only ever been called
//! directly from unit tests.

use dashmap::DashMap;
use pneumatic_core::config::{BootstrapPeer, Config};
use pneumatic_core::crypto::AsymCryptoProvider;
use pneumatic_core::encoding::deserialize_rmp_to;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::{NetworkPacket, NodeRegistryType, NodeTypeConfig};
use pneumatic_core::rns::config_builder::RnsNodeConfigBuilder;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::rns::wrapper::RnsNetwork;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Base ports for the two nodes. Each has exactly one peer, so it binds a
/// single interface at its base port and forwards to the peer's base port — the
/// simplest case of the point-to-point rule, where the `+j` offset is zero for
/// both directions (see `tests/pipeline_integration.rs` for the multi-peer
/// version of that rule).
const PORT_A: u16 = 4710;
const PORT_B: u16 = 4721;

/// How long to wait for the RNS announce handshake to make both directions
/// live, and then for peering to converge. Both match the 20 s the existing
/// integration test allows for the same handshake.
const ROUTE_DEADLINE: Duration = Duration::from_secs(20);
const PEER_DEADLINE: Duration = Duration::from_secs(30);

fn bootstrap_peer(identity: &NodeIdentity, port: u16) -> BootstrapPeer {
    BootstrapPeer {
        // The **RNS** public key in hex, exactly as a launcher would write it
        // into `config.json` — 64 bytes, not the 32-byte Ed25519 key.
        public_key: hex::encode(
            identity
                .rns
                .get_public_key()
                .expect("rns public key"),
        ),
        ip: "127.0.0.1".to_string(),
        port,
    }
}

fn all_types_with_capacity() -> Arc<DashMap<NodeRegistryType, NodeTypeConfig>> {
    let type_configs = DashMap::new();
    for node_type in NodeRegistry::routed_role_types() {
        type_configs.insert(
            node_type,
            NodeTypeConfig {
                min: 0,
                max: 5,
                min_stake: 0,
            },
        );
    }
    Arc::new(type_configs)
}

struct TestNode {
    name: &'static str,
    identity: Arc<NodeIdentity>,
    network: Arc<RnsNetwork>,
    registry: Arc<NodeRegistry>,
}

fn start_node(
    name: &'static str,
    base_port: u16,
    identity: Arc<NodeIdentity>,
    peers: Vec<BootstrapPeer>,
    declared: &[NodeRegistryType],
) -> TestNode {
    let mut builder = RnsNodeConfigBuilder::new().with_udp_port(base_port);
    for peer in &peers {
        builder = builder.add_peer(&peer.ip, peer.port);
    }
    let node_config = builder.build(&identity.rns);

    let network = Arc::new(
        RnsNetwork::start(node_config, &identity, &peers)
            .unwrap_or_else(|e| panic!("[{name}] failed to start RNS transport: {e}")),
    );

    let mut config = Config::new_for_testing(
        "peering_e2e".to_string(),
        Arc::new(DashMap::new()),
        all_types_with_capacity(),
    );
    config.identity = Arc::clone(&identity);
    config.rhash = identity.rhash;
    config.public_key = identity
        .ed25519
        .public_key()
        .expect("ed25519 public key");
    config.node_registry_types = declared.to_vec();
    config.bootstrap_peers = peers;

    let registry = Arc::new(NodeRegistry::init(
        Arc::new(config),
        Some(Arc::clone(&network)),
        Arc::new(|_key, _type| true), // stake gate open: this test is about peering
    ));
    registry.set_declared_roles(declared.to_vec());

    // The same bridge both binaries install: control frames to the registry.
    let registry_for_packets = Arc::clone(&registry);
    network.on_packet(Arc::new(move |raw: Vec<u8>| {
        match deserialize_rmp_to::<NetworkPacket>(&raw) {
            Ok(packet) => {
                if let Some(control) = packet.control {
                    if let Err(e) = registry_for_packets.handle_control(control) {
                        eprintln!("[{name}] control-plane error: {e}");
                    }
                }
            }
            Err(e) => eprintln!("[{name}] dropping undecodable packet: {e}"),
        }
    }));

    TestNode {
        name,
        identity,
        network,
        registry,
    }
}

impl TestNode {
    fn holds(&self, peer: &TestNode, node_type: &NodeRegistryType) -> bool {
        self.registry
            .get_nodes(node_type)
            .expect("type configured")
            .contains_key(&peer.identity.ed25519.public_key().expect("key"))
    }
}

/// A node cannot peer at t=0: it needs a live route, which means it must have
/// heard an announce from the peer. Bootstrap-seeded routes are not live
/// (`received_at == 0.0`). This is the same settle loop the existing
/// integration test uses — re-announce, poll, give up after 20 s.
fn wait_for_live_routes(a: &TestNode, b: &TestNode) {
    let deadline = Instant::now() + ROUTE_DEADLINE;
    loop {
        a.network.announce();
        b.network.announce();
        let a_sees_b = a.network.route_is_live(b.identity.rhash);
        let b_sees_a = b.network.route_is_live(a.identity.rhash);
        if a_sees_b && b_sees_a {
            eprintln!("[peering-e2e] both routes live");
            return;
        }
        if Instant::now() > deadline {
            panic!(
                "[{}→{}] route live: {}; [{}→{}] route live: {}",
                a.name, b.name, a_sees_b, b.name, a.name, b_sees_a
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn two_nodes_peer_over_udp_and_land_in_each_others_role_directories() {
    let identity_a = Arc::new(NodeIdentity::generate_in_memory());
    let identity_b = Arc::new(NodeIdentity::generate_in_memory());
    let peer_a = bootstrap_peer(&identity_a, PORT_A);
    let peer_b = bootstrap_peer(&identity_b, PORT_B);

    // Deliberately *asymmetric* roles: if `RegisterAck` handling still filed
    // the responder under the type named in the ack (which describes the
    // requester), each node would land in the other's own bucket and this test
    // would fail in exactly the way a mis-routed cluster fails.
    let a = start_node(
        "alice",
        PORT_A,
        identity_a,
        vec![peer_b],
        &[NodeRegistryType::Sentinel],
    );
    let b = start_node(
        "bob",
        PORT_B,
        identity_b,
        vec![peer_a],
        &[NodeRegistryType::Committer],
    );

    wait_for_live_routes(&a, &b);

    // Peering, on a fast tick: registration, directory requests, then the
    // heartbeat/re-register cadence.
    assert!(
        a.registry.start_peering_with_tick(Duration::from_millis(500)),
        "alice declared roles and has a transport, so the peering loop must start"
    );
    assert!(b.registry.start_peering_with_tick(Duration::from_millis(500)));

    let deadline = Instant::now() + PEER_DEADLINE;
    loop {
        let a_holds_b = a.holds(&b, &NodeRegistryType::Committer);
        let b_holds_a = b.holds(&a, &NodeRegistryType::Sentinel);
        if a_holds_b && b_holds_a {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "peering did not converge: {} holds {} in its committer bucket: {}; \
                 {} holds {} in its sentinel bucket: {}",
                a.name, b.name, a_holds_b, b.name, a.name, b_holds_a
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // Landed — and in the buckets their roles belong in, not each other's.
    assert!(a.holds(&b, &NodeRegistryType::Committer));
    assert!(b.holds(&a, &NodeRegistryType::Sentinel));
    assert!(
        !a.holds(&b, &NodeRegistryType::Sentinel),
        "bob is a committer; he must not sit in alice's sentinel bucket"
    );
    assert!(
        !b.holds(&a, &NodeRegistryType::Committer),
        "alice is a sentinel; she must not sit in bob's committer bucket"
    );
}
