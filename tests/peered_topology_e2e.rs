//! Peer-derived topology: the cluster's routing table comes from the
//! registration protocol instead of being written by the test.
//!
//! **Why this file exists.** `tests/pipeline_integration.rs`,
//! `tests/shielded_pipeline.rs` and `tests/transport_integration.rs` all build
//! their cluster topology by calling `NodeRegistry::register_peer(key, rhash,
//! node_type, conn)` directly — a **test-only API with no production caller** —
//! and each picks its peers' role buckets by hand. They are right to do it for
//! what they measure: they pin pipeline semantics (sharding, quorum,
//! finalization, commit) and hold that stable against a fixed directory. But a
//! fixture that hand-builds the directory cannot observe the code that is
//! supposed to *derive* it. Three defects in that layer — `RegisterAck` filing a
//! peer under the requester's own role, ~5.9 KB control frames sent on the 481 B
//! direct-packet path, and a directory response that echoed a `Request` — all
//! coexisted with a green suite for exactly this reason
//! (`fact-control-plane-silent-drop-paths`,
//! `fact-register-ack-bucket-placement-defect`).
//!
//! This test closes that gap without duplicating the pipeline tests. Four nodes,
//! one per role, full mesh over real UDP. Nothing calls `register_peer`. Every
//! entry in every directory arrives because a node sent a signed `Register`, a
//! peer verified the binding, ran the stake gate, and chose a bucket from the
//! roles the sender declared. Only then does the test drive real pipeline
//! traffic through `NodeRegistry::send_to_all_blocking`, over fan-out lists it
//! did not author.
//!
//! What that ordering buys: if ack handling regresses so a peer lands in the
//! wrong bucket, the sentinel's Executor directory is empty or holds the wrong
//! node, the "Preload" fan-out delivers to nobody, and the assertion on the
//! receiver's inbox fails. The same is true for a broken frame, an unsent reply,
//! or a binding the receiver fails to verify — every one of which is invisible
//! to a hand-seeded directory.
//!
//! Topology is a **full mesh of 4**, 3 interfaces per node. Not a chain: control
//! frames need a *live route*, and two leaves cannot reach each other through a
//! third (announce retransmit and LRPROOF forwarding are transport-gated in
//! rns-core), so every pair that must register needs a direct link. The port
//! arithmetic follows the j-rule documented in `pipeline_integration.rs:413-426`:
//! interface `k` on a node listens on `base + k` and must forward to
//! `peer_base + j`, where `j` is *this* node's index in *that* peer's peer list.

use dashmap::DashMap;
use pneumatic_core::config::{BootstrapPeer, Config};
use pneumatic_core::crypto::AsymCryptoProvider;
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::{NetworkPacket, NodeRegistryType, NodeType, NodeTypeConfig};
use pneumatic_core::rns::config_builder::RnsNodeConfigBuilder;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::rns::wrapper::RnsNetwork;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One node per role. Every node registers with every other, so each directory
/// is fully derived and the mesh is 3 interfaces per node — well inside the
/// density the transport has been observed to handle reliably.
const ROLES: [(&str, NodeRegistryType); 4] = [
    ("sentinel", NodeRegistryType::Sentinel),
    ("executor", NodeRegistryType::Executor),
    ("finalizer", NodeRegistryType::Finalizer),
    ("committer", NodeRegistryType::Committer),
];

/// The pipeline's real hops: `(from role, action, to role)`. These are the
/// actions the role crates actually fan out, in the order the standard path
/// needs them.
const HOPS: [(&str, &str, &str); 4] = [
    ("sentinel", "Preload", "executor"),
    ("executor", "Sign", "finalizer"),
    ("finalizer", "Commit", "committer"),
    // The finalizer releases the sentinel's in-flight slot; it is the one
    // backward hop, and it is why the mesh (not a chain) is required.
    ("finalizer", "Clear", "sentinel"),
];

const ROUTE_DEADLINE: Duration = Duration::from_secs(25);
const PEER_DEADLINE: Duration = Duration::from_secs(30);
const DELIVERY_DEADLINE: Duration = Duration::from_secs(20);

fn next_port_base() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    use std::sync::atomic::Ordering;
    let pid = std::process::id() as u16;
    let start = 21_000 + (pid % 200) * 64;
    let _ = NEXT.compare_exchange(0, start, Ordering::SeqCst, Ordering::SeqCst);
    loop {
        // Probe UDP: rns-net binds UDP listen sockets, and a port free for TCP
        // may still be taken for UDP. Each node needs one port per peer, so we
        // claim a block of 8 and hand back its first port.
        let base = NEXT.fetch_add(8, Ordering::SeqCst);
        if (0..8u16).all(|i| std::net::UdpSocket::bind(("127.0.0.1", base + i)).is_ok()) {
            return base;
        }
    }
}

fn rns_pub_hex(identity: &NodeIdentity) -> String {
    hex::encode(identity.rns.get_public_key().expect("rns public key"))
}

fn registry_config(identity: &Arc<NodeIdentity>, bootstrap: Vec<BootstrapPeer>) -> Config {
    let type_configs = Arc::new({
        let tc: DashMap<NodeRegistryType, NodeTypeConfig> = DashMap::new();
        for node_type in NodeRegistry::routed_role_types() {
            tc.insert(node_type, NodeTypeConfig { min: 0, max: 10, min_stake: 0 });
        }
        tc
    });
    Config {
        public_key: identity.ed25519.public_key().expect("identity public key"),
        ip_address: "127.0.0.1".parse().expect("loopback"),
        rest_api_version: 1,
        node_type: NodeType::Full,
        // Config declares all four; the registry is then narrowed to this node's
        // actual role. A node advertises `declared_roles`, never this field —
        // over-declaring would file it in buckets whose traffic it cannot serve.
        node_registry_types: vec![
            NodeRegistryType::Sentinel,
            NodeRegistryType::Executor,
            NodeRegistryType::Finalizer,
            NodeRegistryType::Committer,
        ],
        main_environment_id: "peered_topology".to_string(),
        reconciliation_partition_id: "reconciliation".to_string(),
        environment_metadata: Arc::new(DashMap::new()),
        type_configs,
        identity: identity.clone(),
        rhash: identity.rhash,
        bootstrap_peers: bootstrap,
        rns_port: 0,
        transport_enabled: false,
    }
}

struct RoleNode {
    name: &'static str,
    role: NodeRegistryType,
    identity: Arc<NodeIdentity>,
    network: Arc<RnsNetwork>,
    registry: Arc<NodeRegistry>,
    /// Data-plane messages this node actually received over its transport.
    inbox: Arc<Mutex<Vec<Message>>>,
}

impl RoleNode {
    fn directory_contains(&self, node_type: &NodeRegistryType, peer: &RoleNode) -> bool {
        self.registry
            .get_nodes(node_type)
            .expect("type configured")
            .contains_key(&peer.identity.ed25519.public_key().expect("key"))
    }

    fn directory_len(&self, node_type: &NodeRegistryType) -> usize {
        self.registry.get_nodes(node_type).expect("type configured").len()
    }

    /// Did a message with this action, sent by `from`, reach this node?
    fn received(&self, action: &str, from: &RoleNode) -> bool {
        self.inbox
            .lock()
            .expect("inbox lock")
            .iter()
            .any(|m| m.action == action && m.public_key == from.identity.ed25519.public_key().expect("key"))
    }
}

/// Build the full mesh: reserve every port block first (peer ports must be known
/// at bootstrap seed time), then start each node forwarding to
/// `peer_base + j` per the j-rule.
fn start_mesh() -> Vec<RoleNode> {
    let identities: Vec<Arc<NodeIdentity>> = (0..ROLES.len())
        .map(|_| Arc::new(NodeIdentity::generate_in_memory()))
        .collect();
    let bases: Vec<u16> = (0..ROLES.len()).map(|_| next_port_base()).collect();

    // Node `i`'s peer list is every other index, ascending — the same order on
    // every node, which is what makes `j` well defined for both directions.
    let peers_of = |i: usize| -> Vec<usize> {
        (0..identities.len()).filter(|k| *k != i).collect()
    };

    let mut nodes = Vec::new();
    for i in 0..ROLES.len() {
        let name = ROLES[i].0;
        let role = ROLES[i].1.clone();
        let peer_indices = peers_of(i);

        let mut builder = RnsNodeConfigBuilder::new().with_udp_port(bases[i]);
        let mut bootstrap = Vec::new();
        for &k in &peer_indices {
            // Our interface forwards to the port *that peer listens on for us*:
            // its base plus our index in its peer list. Forwarding to
            // `bases[k]` instead makes the link handshake fail silently.
            let j = peers_of(k).iter().position(|&q| q == i).expect("symmetric mesh");
            let port = bases[k] + j as u16;
            builder = builder.add_peer("127.0.0.1", port);
            bootstrap.push(BootstrapPeer {
                public_key: rns_pub_hex(&identities[k]),
                ip: "127.0.0.1".to_string(),
                port,
            });
        }
        let node_config = builder.build(&identities[i].rns);
        let network = Arc::new(
            RnsNetwork::start(node_config, &identities[i], &bootstrap)
                .unwrap_or_else(|e| panic!("[{name}] rns start failed: {e}")),
        );

        let registry = Arc::new(NodeRegistry::init(
            Arc::new(registry_config(&identities[i], bootstrap)),
            Some(Arc::clone(&network)),
            Arc::new(|_key, _type| true),
        ));
        registry.set_declared_roles(vec![role.clone()]);

        // The production bridge shape, with the one branch the other
        // integration tests deliberately omit: control frames reach the registry.
        let registry_for_packets = Arc::clone(&registry);
        let inbox = Arc::new(Mutex::new(Vec::<Message>::new()));
        let inbox_for_packets = Arc::clone(&inbox);
        network.on_packet(Arc::new(move |raw: Vec<u8>| {
            let Ok(packet) = deserialize_rmp_to::<NetworkPacket>(&raw) else {
                return;
            };
            if let Some(control) = packet.control {
                if let Err(e) = registry_for_packets.handle_control(control) {
                    eprintln!("[{name}] control-plane error: {e}");
                }
            }
            // Data-plane only. A directory *response* also arrives as `data`,
            // but a full mesh never needs one — every peer is already a direct
            // bootstrap peer — and decoding one as a `Message` fails closed
            // rather than being mistaken for pipeline traffic.
            if let Some(data) = packet.data {
                if let Ok(message) = deserialize_rmp_to::<Message>(&data) {
                    inbox_for_packets.lock().expect("inbox lock").push(message);
                }
            }
        }));

        nodes.push(RoleNode {
            name,
            role,
            identity: Arc::clone(&identities[i]),
            network,
            registry,
            inbox,
        });
    }
    nodes
}

fn find<'a>(nodes: &'a [RoleNode], name: &str) -> &'a RoleNode {
    nodes.iter().find(|n| n.name == name).expect("node exists")
}

/// Re-announce until every directed edge is live. Nothing can register before
/// this: a bootstrap-seeded route is not a live route, and a control frame is
/// too large for the direct-packet path.
fn wait_for_mesh_to_settle(nodes: &[RoleNode]) {
    let deadline = Instant::now() + ROUTE_DEADLINE;
    loop {
        for node in nodes {
            node.network.announce();
        }
        let mut dead = Vec::new();
        for a in nodes {
            for b in nodes {
                if std::ptr::eq(a, b) {
                    continue;
                }
                if !a.network.route_is_live(b.identity.rhash) {
                    dead.push(format!("{}→{}", a.name, b.name));
                }
            }
        }
        if dead.is_empty() {
            eprintln!("[peered-topology] all 12 directed routes live");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "mesh routes never all became live; still dead: {dead:?}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn a_cluster_derived_by_peering_routes_the_pipelines_real_hops() {
    let nodes = start_mesh();
    wait_for_mesh_to_settle(&nodes);

    // Every node starts its own peering loop. From here on, no test code touches
    // any directory.
    for node in &nodes {
        assert!(
            node.registry.start_peering_with_tick(Duration::from_millis(500)),
            "[{}] has a transport and a declared role, so peering must start",
            node.name
        );
    }

    // --- Phase 1: the directories must be *derived*, and derived correctly ---
    let deadline = Instant::now() + PEER_DEADLINE;
    loop {
        let converged = nodes.iter().all(|node| {
            nodes.iter().all(|peer| {
                std::ptr::eq(node, peer) || node.directory_contains(&peer.role, peer)
            })
        });
        if converged {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "peering did not converge: {:?}",
            nodes
                .iter()
                .map(|n| format!("{}={}", n.name, NodeRegistry::routed_role_types()
                    .iter().map(|t| format!("{t:?}:{}", n.directory_len(t))).collect::<Vec<_>>().join(",")))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // Each node holds each peer in the bucket for the role that peer declared —
    // and nowhere else. With one node per role, a node's bucket for *its own*
    // role is empty: it is not its own peer. So exactly one entry per foreign
    // bucket, zero in the home bucket. A node filing itself anywhere, or filing
    // a peer under the ack's type instead of the peer's declared roles, breaks
    // these counts.
    for node in &nodes {
        for node_type in NodeRegistry::routed_role_types() {
            let expected = usize::from(node_type != node.role);
            assert_eq!(
                node.directory_len(&node_type),
                expected,
                "[{}] bucket {node_type:?} should hold {} (is home role: {})",
                node.name,
                expected,
                node_type == node.role,
            );
        }
    }

    // --- Phase 2: drive the pipeline's hops over those derived directories ---
    // Each fan-out is the production `send_to_all_blocking`, addressed only from
    // the sender's own directory. The test names a role and an action; it never
    // names a destination address.
    for (from_name, action, to_name) in HOPS {
        let from = find(&nodes, from_name);
        let to = find(&nodes, to_name);
        let message = Message::signed(
            "peered_topology".to_string(),
            action,
            format!("{from_name}->{to_name}:{action}").into_bytes(),
            None,
            &from.identity,
        )
        .expect("signed message");
        let payload = serialize_to_bytes_rmp(&message).expect("serialize message");

        from
            .registry
            .send_to_all_blocking(payload, &to.role);
    }

    let deadline = Instant::now() + DELIVERY_DEADLINE;
    loop {
        let missing: Vec<String> = HOPS
            .iter()
            .filter_map(|(from_name, action, to_name)| {
                let from = find(&nodes, from_name);
                let to = find(&nodes, to_name);
                if to.received(action, from) {
                    None
                } else {
                    Some(format!("{from_name} --{action}--> {to_name}"))
                }
            })
            .collect();
        if missing.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "traffic over the peer-derived directories never arrived: {missing:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
