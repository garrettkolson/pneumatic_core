//! Public e2e pipeline integration test — the **standard transaction path**
//! (sentinel → executor → finalizer → committer) over the **production RNS
//! wire**, with at least two nodes per role.
//!
//! ```text
//! submitter --"Process"--> S1, S2  (Sentinel)
//! S1, S2     --"Preload"--> E1, E2 (Executor)
//! E1, E2     --"Sign"------> F1, F2 (Finalizer)
//! F1, F2     --"Commit" / "BlockFinalized"--> C1, C2 (Committer)
//! F1, F2     --"Clear"------> S1, S2  (Sentinel)
//! ```
//!
//! Transport topology. RNS is the production inter-node wire, and its UDP
//! interfaces are point-to-point: a node with N peers binds N listen ports
//! (`udp_port + 0 .. udp_port + N-1`), each forwarding to one peer's base
//! port. A dense 9-node mesh (up to 6 interfaces/node) proved unreliable in
//! practice — a stable subset of directed routes never became live no matter
//! how long the nodes re-announced (see the rns-net spike notes). So the test
//! rides a **5-RNS-node chain** with at most 3 interfaces per node, which is
//! close to the reliably-working 2-node 7.1 transport test:
//!
//! ```text
//! N_SUB ─ N_SENT ─ N_EXEC ─ N_FIN ─ N_COMM
//!             ▲                  │
//!             └──── Clear ───────┘
//! ```
//!
//! Each RNS node hosts its role instances (two per role): `N_SENT` runs S1 and
//! S2, `N_EXEC` runs E1 and E2, and so on. Every hop between roles is a real
//! RNS link; a packet the RNS node receives is fanned out to every role
//! instance on that node (mirroring the composite node-server's `on_packet`
//! bridge). Role instances keep their own identities, registries, and auth
//! boundaries (sentinel sender binding, finalizer C1 gate, committer envelope
//! + role gate) — only the transport is shared.
//!
//! This is the last piece of the audit's overall Done-when: a public test
//! proving the standard pipeline commits end-to-end on the wire the
//! production nodes actually use. Audit 7.1 found the RNS path untested; the
//! ~3.8 KB `Message` framing/MTU gap that broke it (direct-packet cap ~481 B)
//! is fixed by `RnsNetwork::send_data_packet` (data-plane framing + Resource
//! routing above the cap) and exercised here for real.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pneumatic_core::conns::Connection;
use pneumatic_core::config::{BootstrapPeer, Config};
use pneumatic_core::crypto::{AsymCryptoProvider, BasicHashProvider};
use pneumatic_core::data::{DataError, DataProvider, StubDataProvider};
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
use pneumatic_core::epoch::{
    BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector, ExecutorSet, StakeSet,
};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::{NetworkPacket, NodeRegistryType, NodeType, NodeTypeConfig};
use pneumatic_core::registry::{PendingTransactionRegistry, TransactionSignatureRegistry};
use pneumatic_core::rns::conn::RnsConnection;
use pneumatic_core::rns::config_builder::RnsNodeConfigBuilder;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::rns::wrapper::RnsNetwork;
use pneumatic_core::tokens::Token;
use pneumatic_core::transactions::{Transaction, TransactionSignature};
use pneumatic_core::user::User;
use pneumatic_core::validation::ValidationSpecRegistry;
use pneumatic_committer::block_services::BlockServices;
use pneumatic_committer::epoch_manager::{
    EpochReconciler, LeaderSelector, StakeStore, StakingManager,
};
use pneumatic_committer::{Committer, ShieldedPool};
use pneumatic_executor::Executor;
use pneumatic_finalizer::finalizer::Finalizer;
use pneumatic_sentinel::sentinel::Sentinel;
use tokio::time::sleep;

const ENV_ID: &str = "test_env";
const TOKEN_ID: [u8; 1] = [1];
const PARTITION: &str = "token";
const TX_ID: &str = "e2e_std_tx_1";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Find a free loopback port (the `committer/tests/pipeline_integration.rs`
/// 7.1 pattern: per-process atomic base + bind probe; rns-net binds its own
/// UDP socket on the returned port).
fn find_free_port() -> u16 {
    static NEXT_PORT: AtomicU16 = AtomicU16::new(0);
    let pid = std::process::id() as u16;
    let base = 20_000 + (pid % 250) * 160;
    let _ = NEXT_PORT.compare_exchange(0, base, Ordering::SeqCst, Ordering::SeqCst);
    loop {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// The shared test environment (the `tests/shielded_pipeline.rs` S6.1 env):
/// `max_risk: 1.0` so the small standard transfer passes the risk gate, and
/// the default spec registry (`SelfSigned` + `Executed`) installed as the
/// transaction-validation specs.
fn test_env() -> Arc<EnvironmentMetadata> {
    let json = r#"{
        "environment_id": "test_env",
        "environment_name": "test_env",
        "partitions": [
            {"id": "token", "partition_type": "Token"},
            {"id": "slush", "partition_type": "Slush"},
            {"id": "reconciliation", "partition_type": "Other"}
        ],
        "asym_crypto_provider": {"Ed25519": null},
        "sym_crypto_provider": "AES",
        "serialization_provider": "MsgPack",
        "quorum_percentage": 67,
        "override_quorum_percentage": 0.0,
        "security_level": 2,
        "chain_count": 2,
        "node_registry_type": 0,
        "max_stake": 0,
        "min_stake": 0,
        "crypto_provider": "BasicHashProvider",
        "blockchain_metadata": [],
        "block_validators": [],
        "data_provider": "DefaultDataProvider",
        "rest_api_version": 1,
        "is_full_node": true,
        "is_light_node": false,
        "max_in_flight": 100,
        "max_gas_limit": 1000000,
        "max_risk": 1.0,
        "allowed_token_types": [],
        "trans_validation_specs": [],
        "block_validation_specs": [],
        "log_file": "test.log",
        "logger": "FileLogger"
    }"#;
    let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).expect("valid test env JSON");
    let mut env = EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec");
    let mut registry = ValidationSpecRegistry::new();
    registry.register_defaults();
    env.transaction_validation_specs = Arc::new(registry);
    Arc::new(env)
}

/// Per-node `Config` (the S6.1 `role_config` pattern): capacity 10 for every
/// registry type so `register_peer` never hits the maxed-out branch.
fn role_config(identity: &Arc<NodeIdentity>) -> Config {
    let type_configs = Arc::new({
        let tc = dashmap::DashMap::new();
        for t in [
            NodeRegistryType::Sentinel.clone(),
            NodeRegistryType::Executor.clone(),
            NodeRegistryType::Finalizer.clone(),
            NodeRegistryType::Committer.clone(),
            NodeRegistryType::Archiver.clone(),
        ] {
            tc.insert(t, NodeTypeConfig { min: 0, max: 10, min_stake: 0 });
        }
        tc
    });
    Config {
        public_key: identity.ed25519.public_key().expect("identity public key"),
        ip_address: "127.0.0.1".parse().expect("localhost"),
        rest_api_version: 1,
        node_type: NodeType::Full,
        node_registry_types: vec![
            NodeRegistryType::Sentinel,
            NodeRegistryType::Executor,
            NodeRegistryType::Finalizer,
            NodeRegistryType::Committer,
        ],
        main_environment_id: ENV_ID.to_string(),
        reconciliation_partition_id: "reconciliation".to_string(),
        environment_metadata: Arc::new(dashmap::DashMap::new()),
        type_configs,
        identity: identity.clone(),
        rhash: identity.rhash,
        bootstrap_peers: Vec::new(),
        rns_port: 0,
        transport_enabled: false,
    }
}

/// The standard (executed) token: NOT self-verified (routes through the full
/// pipeline), `Executed` block-validation spec, and an **empty blockchain** —
/// so `resolve_previous_hash` yields an empty prev-hash and the block the
/// finalizer proposes is a valid genesis on the committers' empty chains
/// (the strict linkage check requires an empty `previous_hash` on an empty
/// chain).
fn standard_token() -> Token {
    let mut token = Token::new();
    token.id = TOKEN_ID.to_vec();
    token.is_self_verified = false;
    token.block_validation_spec_name = "Executed".to_string();
    token
}

/// Data provider for the wire test: the S6.1 `StubDataProvider` (token, user,
/// stake snapshots) plus a working `get_data` — the executor's
/// `run_execution` fetches contract + sender data via `get_data` and the stub
/// returns `DataNotFound` unconditionally.
struct TestDataProvider {
    inner: StubDataProvider,
    data: Mutex<std::collections::HashMap<Vec<u8>, Vec<u8>>>,
}

impl DataProvider for TestDataProvider {
    fn get_token(&self, key: &Vec<u8>, partition_id: &str) -> Result<Token, DataError> {
        self.inner.get_token(key, partition_id)
    }
    fn save_token(&self, key: &Vec<u8>, token: Token, partition_id: &str) -> Result<(), DataError> {
        self.inner.save_token(key, token, partition_id)
    }
    fn get_data(&self, key: &Vec<u8>, _partition_id: &str) -> Result<Vec<u8>, DataError> {
        self.data
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or(DataError::DataNotFound)
    }
    fn save_data(&self, key: &Vec<u8>, data: Vec<u8>, partition_id: &str) -> Result<(), DataError> {
        self.inner.save_data(key, data, partition_id)
    }
    fn get_user(&self, key: &Vec<u8>, partition_id: &str) -> Result<User, DataError> {
        self.inner.get_user(key, partition_id)
    }
    fn save_user(&self, key: &Vec<u8>, user: User, partition_id: &str) -> Result<(), DataError> {
        self.inner.save_user(key, user, partition_id)
    }
    fn get_stake_snapshot(&self, epoch: u64, partition_id: &str) -> Result<StakeSet, DataError> {
        self.inner.get_stake_snapshot(epoch, partition_id)
    }
    fn save_stake_snapshot(&self, epoch: u64, snapshot: StakeSet, partition_id: &str) -> Result<(), DataError> {
        self.inner.save_stake_snapshot(epoch, snapshot, partition_id)
    }
    fn get_executor_set(&self, epoch: u64, partition_id: &str) -> Result<ExecutorSet, DataError> {
        self.inner.get_executor_set(epoch, partition_id)
    }
    fn save_executor_set(&self, epoch: u64, set: ExecutorSet, partition_id: &str) -> Result<(), DataError> {
        self.inner.save_executor_set(epoch, set, partition_id)
    }
    fn latest_block_hash(&self, partition_id: &str) -> Result<Option<Vec<u8>>, DataError> {
        self.inner.latest_block_hash(partition_id)
    }
}

// ---------------------------------------------------------------------------
// RNS node harness
// ---------------------------------------------------------------------------

fn rns_pub_hex(id: &NodeIdentity) -> String {
    hex::encode(id.rns.get_public_key().expect("rns public key"))
}

/// One RNS node: identity + UDP port + started `RnsNetwork` bootstrapping
/// `peers` (symmetric topology — every listed peer bootstraps us back, so the
/// announce handshake activates the send routes in both directions).
struct RnsNode {
    name: &'static str,
    identity: Arc<NodeIdentity>,
    port: u16,
    network: Arc<RnsNetwork>,
}

impl RnsNode {
    fn start(name: &'static str, identity: &Arc<NodeIdentity>, port: u16, peers: &[(String, u16)]) -> RnsNode {
        let mut builder = RnsNodeConfigBuilder::new().with_udp_port(port);
        let mut bootstrap = Vec::new();
        for (hex_key, peer_port) in peers {
            builder = builder.add_peer("127.0.0.1", *peer_port);
            bootstrap.push(BootstrapPeer {
                public_key: hex_key.clone(),
                ip: "127.0.0.1".to_string(),
                port: *peer_port,
            });
        }
        let node_config = builder.build(&identity.rns);
        let network = Arc::new(
            RnsNetwork::start(node_config, identity, &bootstrap)
                .unwrap_or_else(|e| panic!("{name}: rns start failed: {e}")),
        );
        RnsNode { name, identity: identity.clone(), port, network }
    }

    /// Install the composite-style data-plane bridge: decrypted raw frame →
    /// `NetworkPacket` → `data` → `Message` → role dispatch. Undecodable
    /// frames and control-only packets are dropped (the node-server bridge
    /// shape; this test sends no control-plane traffic — peers are
    /// registered directly in each `NodeRegistry`).
    fn install_bridge<F: Fn(Message) + Send + Sync + 'static>(&self, dispatch: F) {
        self.network.on_packet(Arc::new(move |raw: Vec<u8>| {
            let Ok(packet) = deserialize_rmp_to::<NetworkPacket>(&raw) else {
                return;
            };
            let Some(data) = packet.data else { return };
            let Ok(message) = deserialize_rmp_to::<Message>(&data) else {
                return;
            };
            dispatch(message);
        }));
    }

    /// A registry `Connection` to `peer` over THIS node's network (the
    /// production `RnsConnection`: `send` → `send_data_packet`, which frames
    /// the payload in a `NetworkPacket` and routes above-MTU frames through
    /// the Resource path).
    fn conn_to(&self, peer: &NodeIdentity) -> Box<dyn Connection> {
        Box::new(RnsConnection::new(peer.rhash, Arc::clone(&self.network)))
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_standard_pipeline_commits_over_rns() {
    let env = test_env();

    // --- Identities: 8 role nodes + 1 submitter. Five of them also serve as
    //     their RNS node's wire identity: S1→N_SENT, E1→N_EXEC, F1→N_FIN,
    //     C1→N_COMM, submitter→N_SUB. The second instance of each role (S2,
    //     E2, F2, C2) shares its RNS node and is reached via the node's
    //     rhash, fanned out in the node's `on_packet` bridge. ---
    let ids: Vec<Arc<NodeIdentity>> = (0..9)
        .map(|_| Arc::new(NodeIdentity::generate_in_memory()))
        .collect();
    let s1_id = &ids[0];
    let s2_id = &ids[1];
    let e1_id = &ids[2];
    let e2_id = &ids[3];
    let f1_id = &ids[4];
    let f2_id = &ids[5];
    let c1_id = &ids[6];
    let c2_id = &ids[7];
    let submitter_id = &ids[8];

    // --- Start the 5 RNS networks as a chain (see module docs for the
    //     topology). Two-phase: reserve all ports first so every node knows
    //     every peer's port before its own network starts (RNS bootstrap is
    //     seed-time). Each node gets its own collision-free 8-port block
    //     (rns-net binds one UDP listen port per peer). ---
    let ports: Vec<u16> = {
        let mut next = find_free_port();
        let mut blocks = Vec::new();
        while blocks.len() < 5 {
            // Probe UDP specifically: rns-net binds UDP listen sockets, and a
            // port can be free for TCP but occupied for UDP (and vice versa).
            let block_free = (0..8u16)
                .all(|i| std::net::UdpSocket::bind(("127.0.0.1", next + i)).is_ok());
            if block_free {
                blocks.push(next);
                next += 8;
            } else {
                next += 8;
            }
        }
        blocks
    };
    // RNS node → (identity, peer identities). The chain:
    //   N_SUB(submitter) ─ N_SENT(s1) ─ N_EXEC(e1) ─ N_FIN(f1) ─ N_COMM(c1)
    //   N_FIN ─ N_SENT  (the Clear return path)
    let rns_nodes: [(&'static str, &Arc<NodeIdentity>, Vec<&Arc<NodeIdentity>>); 5] = [
        ("SUB", submitter_id, vec![s1_id]),
        // SENT needs a direct link to FIN (the finalizer's "Clear" fan-out
        // targets the sentinels; the Resource path requires a direct link).
        ("SENT", s1_id, vec![submitter_id, e1_id, f1_id]),
        ("EXEC", e1_id, vec![s1_id, f1_id]),
        // FIN needs a direct link to SENT (symmetric with SENT's link above).
        ("FIN", f1_id, vec![e1_id, c1_id, s1_id]),
        ("COMM", c1_id, vec![f1_id]),
    ];
    let hex_table: Vec<String> = ids.iter().map(|id| rns_pub_hex(id)).collect();
    let node_ports: [u16; 5] = [ports[0], ports[1], ports[2], ports[3], ports[4]];
    // Which RNS node anchors each of the 9 identities (0=SUB,1=SENT,2=EXEC,
    // 3=FIN,4=COMM). The second instance of each role shares its anchor.
    let anchor_of = |id: &Arc<NodeIdentity>| -> usize {
        let idx = ids.iter().position(|x| Arc::as_ptr(x) == Arc::as_ptr(id)).unwrap();
        match idx {
            8 => 0, // submitter
            0 | 1 => 1, // s1 | s2
            2 | 3 => 2, // e1 | e2
            4 | 5 => 3, // f1 | f2
            6 | 7 => 4, // c1 | c2
            _ => unreachable!(),
        }
    };
    let mut rns = Vec::new();
    for (i, (name, id, peers)) in rns_nodes.iter().enumerate() {
        let peer_list: Vec<(String, u16)> = peers
            .iter()
            .map(|p| {
                let idx = ids.iter().position(|x| Arc::as_ptr(x) == Arc::as_ptr(*p)).unwrap();
                let anchor = anchor_of(p);
                // CRITICAL PORT WIRING: rns-net's UDP interface `k` on a node
                // listens on `base + k` and forwards to the peer's base port.
                // The peer's interface for *us* listens on `peer_base + j`
                // where `j` is *our* index in the peer's peer list. So our
                // interface must forward to `peer_base + j` (not `peer_base`),
                // or the peer receives our packets on the wrong interface and
                // the link handshake silently fails. `j` = our RNS node index
                // in the peer's anchor RNS node's peer list.
                let j = rns_nodes[anchor]
                    .2
                    .iter()
                    .position(|q| anchor_of(q) == i)
                    .expect("symmetric peer list");
                (hex_table[idx].clone(), node_ports[anchor] + j as u16)
            })
            .collect();
        rns.push(RnsNode::start(name, id, node_ports[i], &peer_list));
    }
    let n_sub = &rns[0];
    let n_sent = &rns[1];
    let n_exec = &rns[2];
    let n_fin = &rns[3];
    let n_comm = &rns[4];

    // Let the announce handshakes settle. The startup announce races peers'
    // listeners coming up (the 7.1 transport test documents this), so after an
    // initial settle every node re-announces until every directed RNS edge is
    // live (upgraded from the bootstrap-seeded synthetic route) — only then
    // can the Resource link (required for the ~3.8 KB frames) be established
    // in both directions.
    sleep(Duration::from_millis(1000)).await;
    let nets: Vec<Arc<RnsNetwork>> = rns.iter().map(|n| n.network.clone()).collect();
    // Directed RNS edges that must be live (initiator needs the destination for
    // `create_link`; the responder needs a route back for the handshake reply):
    let rns_edge_list: Vec<(usize, usize)> = vec![
        (0, 1), (1, 0), // SUB <-> SENT
        (1, 2), (2, 1), // SENT <-> EXEC
        (2, 3), (3, 2), // EXEC <-> FIN
        (3, 4), (4, 3), // FIN <-> COMM
    ];
    let rns_rhashes: Vec<[u8; 16]> = rns.iter().map(|n| n.identity.rhash).collect();
    let route_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        for net in &nets {
            net.announce();
        }
        let all_live = rns_edge_list
            .iter()
            .all(|edge| {
                let (a, b) = *edge;
                nets[a].route_is_live(rns_rhashes[b])
            });
        if !all_live {
            let dead: Vec<String> = rns_edge_list
                .iter()
                .filter(|edge| {
                    let (a, b) = **edge;
                    !nets[a].route_is_live(rns_rhashes[b])
                })
                .map(|edge| {
                    let (a, b) = *edge;
                    format!("{a}→{b}")
                })
                .collect();
            eprintln!("[e2e] dead rns edges: {dead:?}");
        }
        if all_live {
            break;
        }
        assert!(
            Instant::now() < route_deadline,
            "not every RNS chain route became live"
        );
        sleep(Duration::from_millis(500)).await;
    }

    // Let in-flight announce/handshake packets drain before we start forcing
    // Resource links up (the route gate only proves routes are live, not that
    // the link handshakes have settled).
    sleep(Duration::from_millis(2000)).await;

    // Establish and verify every directed RNS link up front (before any data
    // depends on it). The on-demand `send_resource_to` link creation can stall
    // mid-pipeline if a handshake is slow; warming every edge here surfaces a
    // stuck link early and lets later sends reuse the Active link. `ensure_link`
    // retries a failed handshake internally (45 s deadline); if an edge still
    // fails, run extra passes over the remaining edges before giving up — a
    // later pass often succeeds once the mesh is fully settled.
    let mut pending: Vec<(usize, usize)> = rns_edge_list.clone();
    for pass in 1..=3u32 {
        let mut still: Vec<(usize, usize)> = Vec::new();
        for &(a, b) in &pending {
            match nets[a].ensure_link(rns_rhashes[b]) {
                Ok(()) => eprintln!("[e2e] link established {a}→{b} (pass {pass})"),
                Err(e) => {
                    eprintln!("[e2e] link {a}→{b} failed on pass {pass}: {e}");
                    still.push((a, b));
                }
            }
        }
        pending = still;
        if pending.is_empty() {
            break;
        }
        eprintln!("[e2e] pass {pass}: {} link(s) pending: {pending:?}", pending.len());
        sleep(Duration::from_millis(2000)).await;
    }
    assert!(
        pending.is_empty(),
        "RNS links not established after 3 passes: {pending:?}"
    );

    // --- Shared fixtures for the data providers ---
    let sender_pk = submitter_id.ed25519.public_key().expect("submitter public key");
    let stake_set = StakeSet {
        stakers: [
            (e1_id.ed25519.public_key().expect("e1 pk"), 100u64),
            (e2_id.ed25519.public_key().expect("e2 pk"), 100u64),
        ]
        .into_iter()
        .collect(),
    };

    /// Per-node data provider: token + sender user + stake snapshots for
    /// epochs 0 and 1 (the finalizer reads epoch 0; the sentinel's
    /// `current_epoch` defaults to 1) + the executor's `get_data` payloads
    /// (contract + sender bytes).
    fn make_dp(sender_pk: Vec<u8>, stake_set: StakeSet) -> Arc<TestDataProvider> {
        let stub = StubDataProvider::new()
            .with_token(TOKEN_ID.to_vec(), PARTITION.to_string(), standard_token())
            .with_user(
                sender_pk.clone(),
                PARTITION.to_string(),
                User {
                    public_key: sender_pk.clone(),
                    fuel_balance: 1_000_000,
                    stake: 0,
                    nonce: 1,
                },
            )
            .with_stake_snapshot(0, stake_set.clone())
            .with_stake_snapshot(1, stake_set);
        Arc::new(TestDataProvider {
            inner: stub,
            data: Mutex::new(
                [
                    (TOKEN_ID.to_vec(), b"contract-bytes".to_vec()),
                    (sender_pk, b"user-data".to_vec()),
                ]
                .into_iter()
                .collect(),
            ),
        })
    }

    fn make_registry(identity: &Arc<NodeIdentity>) -> Arc<NodeRegistry> {
        Arc::new(NodeRegistry::init(
            Arc::new(role_config(identity)),
            None,
            Arc::new(|_, _| true),
        ))
    }

    // --- Sentinels S1, S2 (both on N_SENT). Registry peer: the executor RNS
    //     node (e1_id.rhash) — a single connection; N_EXEC fans the Preload
    //     out to both E1 and E2. ---
    let s1_registry = make_registry(s1_id);
    let s2_registry = make_registry(s2_id);
    for (registry, node, peer) in [
        (s1_registry.as_ref(), n_sent, e1_id),
        (s2_registry.as_ref(), n_sent, e1_id),
    ] {
        assert!(
            registry.register_peer(
                peer.ed25519.public_key().expect("executor pk"),
                peer.rhash,
                &NodeRegistryType::Executor,
                node.conn_to(peer),
            ),
            "sentinel: executor peer registers"
        );
    }

    let s1_pending = Arc::new(PendingTransactionRegistry::new());
    let s2_pending = Arc::new(PendingTransactionRegistry::new());
    let s1_sentinel = Arc::new(Sentinel::new(
        role_config(s1_id),
        env.clone(),
        s1_registry.clone(),
        s1_pending.clone(),
        Arc::new(Gossiper::new(
            NodeRegistryType::Sentinel,
            role_config(s1_id),
            300,
            env.asym_crypto_provider.clone(),
        )),
        make_dp(sender_pk.clone(), stake_set.clone()),
        Arc::new(ShieldedPool::new(10)),
    ));
    let s2_sentinel = Arc::new(Sentinel::new(
        role_config(s2_id),
        env.clone(),
        s2_registry.clone(),
        s2_pending.clone(),
        Arc::new(Gossiper::new(
            NodeRegistryType::Sentinel,
            role_config(s2_id),
            300,
            env.asym_crypto_provider.clone(),
        )),
        make_dp(sender_pk.clone(), stake_set.clone()),
        Arc::new(ShieldedPool::new(10)),
    ));

    // --- Executors E1, E2 (both on N_EXEC). Registry peer: the finalizer RNS
    //     node (f1_id.rhash) — N_FIN fans the Sign vote out to both F1 and F2. ---
    let e1_registry = make_registry(e1_id);
    let e2_registry = make_registry(e2_id);
    for (registry, node, peer) in [
        (e1_registry.as_ref(), n_exec, f1_id),
        (e2_registry.as_ref(), n_exec, f1_id),
    ] {
        assert!(
            registry.register_peer(
                peer.ed25519.public_key().expect("finalizer pk"),
                peer.rhash,
                &NodeRegistryType::Finalizer,
                node.conn_to(peer),
            ),
            "executor: finalizer peer registers"
        );
    }

    let e1_executor = Arc::new(Executor::new(
        ENV_ID.to_string(),
        e1_id.ed25519.public_key().expect("e1 pk"),
        e1_id.clone(),
        e1_registry.clone(),
        make_dp(sender_pk.clone(), stake_set.clone()),
        Arc::new(PendingTransactionRegistry::new()),
        Arc::new(BasicHashProvider::new()),
        100,
    ));
    let e2_executor = Arc::new(Executor::new(
        ENV_ID.to_string(),
        e2_id.ed25519.public_key().expect("e2 pk"),
        e2_id.clone(),
        e2_registry.clone(),
        make_dp(sender_pk.clone(), stake_set.clone()),
        Arc::new(PendingTransactionRegistry::new()),
        Arc::new(BasicHashProvider::new()),
        100,
    ));

    // --- Finalizers F1, F2 (both on N_FIN). Registry peers: both executors
    //     (the C1 gate looks voters up by ed25519 pk — both are on N_EXEC, so
    //     both carry e1_id.rhash); the committer RNS node (commit fan-out);
    //     the sentinel RNS node (Clear fan-out). ---
    let f1_registry = make_registry(f1_id);
    let f2_registry = make_registry(f2_id);
    for (registry, node) in [
        (f1_registry.as_ref(), n_fin),
        (f2_registry.as_ref(), n_fin),
    ] {
        // Executors (C1 gate lookup — both on N_EXEC, rhash e1_id.rhash).
        for peer in [e1_id, e2_id] {
            assert!(
                registry.register_peer(
                    peer.ed25519.public_key().expect("executor pk"),
                    e1_id.rhash,
                    &NodeRegistryType::Executor,
                    node.conn_to(e1_id),
                ),
                "finalizer: executor peer registers"
            );
        }
        // Committers (commit fan-out — both on N_COMM, rhash c1_id.rhash).
        for peer in [c1_id, c2_id] {
            assert!(
                registry.register_peer(
                    peer.ed25519.public_key().expect("committer pk"),
                    c1_id.rhash,
                    &NodeRegistryType::Committer,
                    node.conn_to(c1_id),
                ),
                "finalizer: committer peer registers"
            );
        }
        // Sentinels (Clear fan-out — both on N_SENT, rhash s1_id.rhash).
        for peer in [s1_id, s2_id] {
            assert!(
                registry.register_peer(
                    peer.ed25519.public_key().expect("sentinel pk"),
                    s1_id.rhash,
                    &NodeRegistryType::Sentinel,
                    node.conn_to(s1_id),
                ),
                "finalizer: sentinel peer registers"
            );
        }
    }

    // The finalizer MUST sign blocks with its identity key: the committer
    // verifies `block.signed_trans.finalizer_sig` against
    // `block.signed_trans.finalizer_addr` (derived from the verifying key),
    // so the signing identity and the verifying key must be the same keypair.
    // The `BlockBuilder` now signs with the identity's hybrid provider
    // (producing the `[Ed25519 · ML-DSA pk · ML-DSA sig]` hybrid signature
    // the committer's `check_signature` verifies).
    let f1_verifying = {
        let pk = f1_id.ed25519.public_key().expect("f1 pk");
        let pk_bytes: [u8; 32] = pk.try_into().expect("32-byte pk");
        ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes).expect("valid vk")
    };
    let f2_verifying = {
        let pk = f2_id.ed25519.public_key().expect("f2 pk");
        let pk_bytes: [u8; 32] = pk.try_into().expect("32-byte pk");
        ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes).expect("valid vk")
    };
    let f1_finalizer = Arc::new(Finalizer::new(
        ENV_ID.to_string(),
        f1_id.ed25519.public_key().expect("f1 pk"),
        f1_id.clone(),
        f1_registry.clone(),
        Arc::new(PendingTransactionRegistry::new()),
        Arc::new(TransactionSignatureRegistry::new()),
        67.0,
        1,
        f1_id.clone(),
        f1_verifying,
        Arc::new(BasicHashProvider::new()),
        e1_id.ed25519.public_key().expect("e1 pk"),
        100,
        vec![1u8, 2, 3],
        0,
        make_dp(sender_pk.clone(), stake_set.clone()),
        PARTITION.to_string(),
        Arc::new(ShieldedPool::new(10)),
        env.clone(),
    ));
    let f2_finalizer = Arc::new(Finalizer::new(
        ENV_ID.to_string(),
        f2_id.ed25519.public_key().expect("f2 pk"),
        f2_id.clone(),
        f2_registry.clone(),
        Arc::new(PendingTransactionRegistry::new()),
        Arc::new(TransactionSignatureRegistry::new()),
        67.0,
        1,
        f2_id.clone(),
        f2_verifying,
        Arc::new(BasicHashProvider::new()),
        e2_id.ed25519.public_key().expect("e2 pk"),
        100,
        vec![4u8, 5, 6],
        0,
        make_dp(sender_pk.clone(), stake_set.clone()),
        PARTITION.to_string(),
        Arc::new(ShieldedPool::new(10)),
        env.clone(),
    ));

    // --- Committers C1, C2 (both on N_COMM). Registry peers: the finalizer
    //     RNS node (envelope auth + role gate for "Commit"/"BlockFinalized"). ---
    let c1_registry = make_registry(c1_id);
    let c2_registry = make_registry(c2_id);
    for (registry, node) in [
        (c1_registry.as_ref(), n_comm),
        (c2_registry.as_ref(), n_comm),
    ] {
        for peer in [f1_id, f2_id] {
            assert!(
                registry.register_peer(
                    peer.ed25519.public_key().expect("finalizer pk"),
                    f1_id.rhash,
                    &NodeRegistryType::Finalizer,
                    node.conn_to(f1_id),
                ),
                "committer: finalizer peer registers"
            );
        }
    }

    let c1_tokens = token_cache();
    let c2_tokens = token_cache();
    let c1_committer = Arc::new(build_committer(
        c1_id,
        env.clone(),
        c1_registry.clone(),
        c1_tokens.clone(),
        make_dp(sender_pk.clone(), stake_set.clone()),
    ));
    let c2_committer = Arc::new(build_committer(
        c2_id,
        env.clone(),
        c2_registry.clone(),
        c2_tokens.clone(),
        make_dp(sender_pk.clone(), stake_set.clone()),
    ));

    // --- Install the on_packet bridges (composite node-server shape). Each
    //     RNS node fans a received Message out to every role instance it
    //     hosts. The bridge callbacks run on RNS driver threads (no Tokio
    //     context), so capture the test runtime's handle and use it to spawn
    //     the async role handlers. ---
    let rt = tokio::runtime::Handle::current();
    // N_SENT: every message → both sentinels (they route by action).
    {
        let s1 = s1_sentinel.clone();
        let s2 = s2_sentinel.clone();
        n_sent.install_bridge(move |message| {
            let raw = serialize_to_bytes_rmp(&message).expect("serialize message");
            for sentinel in [s1.clone(), s2.clone()] {
                if let Err(e) = sentinel.on_data_received(raw.clone()) {
                    eprintln!("[e2e] sentinel on_data_received error: {e:?}");
                }
            }
        });
    }
    // N_EXEC: "Preload" → both executors.
    {
        let e1 = e1_executor.clone();
        let e2 = e2_executor.clone();
        let rt = rt.clone();
        n_exec.install_bridge(move |message| {
            if message.action != "Preload" {
                return;
            }
            let body = message.body;
            for executor in [e1.clone(), e2.clone()] {
                let executor = executor.clone();
                let body = body.clone();
                rt.spawn(async move {
                    if let Err(e) = executor.ingest_preload(&body).await {
                        eprintln!("[e2e] executor.ingest_preload error: {e:?}");
                    }
                });
            }
        });
    }
    // N_FIN: "Preload" → both finalizers (register the tx); "Sign" → both
    // finalizers (C1 gate + optimistic finalize on the first vote).
    {
        let f1 = f1_finalizer.clone();
        let f2 = f2_finalizer.clone();
        let rt = rt.clone();
        n_fin.install_bridge(move |message| {
            let action = message.action.clone();
            for finalizer in [f1.clone(), f2.clone()] {
                let finalizer = finalizer.clone();
                let message = message.clone();
                let action = action.clone();
                rt.spawn(async move {
                    match action.as_str() {
                        "Preload" => {
                            if let Err(e) = finalizer.handle_preload(&message).await {
                                eprintln!("[e2e] finalizer.handle_preload error: {e:?}");
                            }
                        }
                        "Sign" => {
                            if let Err(e) = finalizer.handle_signature(&message).await {
                                eprintln!("[e2e] finalizer.handle_signature rejected: {e:?}");
                            }
                        }
                        _ => {}
                    }
                });
            }
        });
    }
    // N_COMM: everything → both committers (envelope auth + role gate).
    {
        let c1 = c1_committer.clone();
        let c2 = c2_committer.clone();
        let rt = rt.clone();
        n_comm.install_bridge(move |message| {
            for committer in [c1.clone(), c2.clone()] {
                let committer = committer.clone();
                let message = message.clone();
                rt.spawn(async move {
                    if let Err(e) = committer.handle_message(message).await {
                        eprintln!("[e2e] committer.handle_message error: {e:?}");
                    }
                });
            }
        });
    }

    // --- Drive the pipeline: the submitter signs the tx and the "Process"
    //     envelope (its Ed25519 key IS tx.sender — the sentinel binds the
    //     two) and sends it over RNS to N_SENT, which fans it out to both
    //     sentinels. ---
    let tx = make_transaction(submitter_id);
    let tx_bytes = serialize_to_bytes_rmp(&tx).expect("serialize tx");
    let process_msg = Message::signed(
        PARTITION.to_string(),
        "Process",
        tx_bytes,
        None,
        submitter_id.as_ref(),
    )
    .expect("signed process message");
    let process_payload = serialize_to_bytes_rmp(&process_msg).expect("serialize process message");

    n_sub
        .network
        .send_data_packet(s1_id.rhash, &process_payload)
        .expect("submitter → N_SENT over RNS");

    // --- Assert: both committers commit the SAME block (H12 / consensus) ---
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let c1n = c1_tokens
            .get(&TOKEN_ID.to_vec())
            .map(|t| t.blockchain.get_count())
            .unwrap_or(0);
        let c2n = c2_tokens
            .get(&TOKEN_ID.to_vec())
            .map(|t| t.blockchain.get_count())
            .unwrap_or(0);
        if c1n >= 1 && c2n >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "committers did not converge (C1={c1n}, C2={c2n}) — pipeline stalled"
        );
        sleep(Duration::from_millis(250)).await;
    }

    let c1_token = c1_tokens.get(&TOKEN_ID.to_vec()).expect("C1 has token").clone();
    let c2_token = c2_tokens.get(&TOKEN_ID.to_vec()).expect("C2 has token").clone();
    let c1_block = c1_token
        .blockchain
        .last_block()
        .expect("C1 committed a block")
        .clone();
    let c2_block = c2_token
        .blockchain
        .last_block()
        .expect("C2 committed a block")
        .clone();

    let c1_tip = c1_tokens
        .get(&TOKEN_ID.to_vec())
        .unwrap()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;
    let c2_tip = c2_tokens
        .get(&TOKEN_ID.to_vec())
        .unwrap()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;

    // Consensus: identical non-empty tip on both committers.
    assert!(!c1_tip.is_empty(), "C1 chain tip is empty");
    assert_eq!(c1_tip, c2_tip, "committers disagree on the chain tip");
    assert_eq!(c1_block.current_hash, c1_tip);
    assert_eq!(c2_block.current_hash, c2_tip);

    // Block content: the right transaction, genesis linkage, executor +
    // finalizer signatures present (the Executed spec's gates).
    assert_eq!(c1_block.signed_trans.transaction.id, TX_ID);
    assert_eq!(
        c1_block.previous_hash.len(),
        0,
        "first block must be a genesis (empty previous_hash)"
    );
    assert!(
        !c1_block.signed_trans.transaction.result_hash.is_empty(),
        "block must carry the executor's result hash"
    );
    assert!(
        !c1_block.signed_trans.executor_sigs.is_empty(),
        "block must carry executor signatures"
    );
    assert!(
        !c1_block.signed_trans.finalizer_sig.signature.is_empty(),
        "block must carry the finalizer signature"
    );

    // --- Assert: "Clear" reached both sentinels (their registries evicted) ---
    let clear_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let s1_cleared = !s1_pending.contains(TX_ID);
        let s2_cleared = !s2_pending.contains(TX_ID);
        if s1_cleared && s2_cleared {
            break;
        }
        assert!(
            Instant::now() < clear_deadline,
            "sentinels never received Clear (S1={s1_cleared}, S2={s2_cleared})"
        );
        sleep(Duration::from_millis(250)).await;
    }

    // --- Adversarial: C1 gate — a forged voter (unregistered key) is
    //     rejected by the finalizer's signature handler. ---
    let rogue = Arc::new(NodeIdentity::generate_in_memory());
    let forged_hash = vec![9u8, 8, 7, 6];
    let forged_sig = TransactionSignature {
        transaction_id: TX_ID.as_bytes().to_vec(),
        env_id: TOKEN_ID.to_vec(),
        transaction_hash: forged_hash.clone(),
        signature: rogue.ed25519.sign_data(&forged_hash).expect("rogue signs"),
        current_stake: 0,
    };
    let forged_msg = Message::signed(
        PARTITION.to_string(),
        "Sign",
        serialize_to_bytes_rmp(&forged_sig).expect("serialize forged sig"),
        None,
        rogue.as_ref(),
    )
    .expect("signed forged message");
    let forged_result = f1_finalizer.handle_signature(&forged_msg).await;
    assert!(
        forged_result.is_err(),
        "finalizer must reject a signature from an unregistered (forged) voter"
    );

    // --- Tear down: drop the roles (releasing their RnsConnection Arcs),
    //     then stop every RNS network. ---
    drop((
        s1_sentinel,
        s2_sentinel,
        s1_registry,
        s2_registry,
        e1_executor,
        e2_executor,
        e1_registry,
        e2_registry,
        f1_finalizer,
        f2_finalizer,
        f1_registry,
        f2_registry,
        c1_committer,
        c2_committer,
        c1_registry,
        c2_registry,
        c1_tokens,
        c2_tokens,
    ));

    for node in &rns {
        let name = node.name;
        let network = node.network.clone();
        match Arc::try_unwrap(network) {
            Ok(node) => node.stop(),
            Err(_arc) => {
                // The RnsConnections inside the (now dropped) registries held
                // extra Arc clones; if any outlived the drop scope the
                // transport is leaked for the test process's lifetime
                // (sockets die with the process).
                eprintln!("[e2e] {name}: rns network still referenced; leaking");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn token_cache() -> Arc<dashmap::DashMap<Vec<u8>, Token>> {
    let map = dashmap::DashMap::new();
    map.insert(TOKEN_ID.to_vec(), standard_token());
    Arc::new(map)
}

fn make_transaction(submitter_id: &Arc<NodeIdentity>) -> Transaction {
    let sender = submitter_id.ed25519.public_key().expect("submitter pk");
    let mut tx = Transaction {
        id: TX_ID.to_string(),
        action: "Transfer".to_string(),
        token_id: TOKEN_ID.to_vec(),
        bid: None,
        sequence_number: 1,
        sender: sender.clone(),
        receiver: vec![2u8],
        amount: Some(100),
        timestamp: 1_700_000_000,
        result_hash: vec![],
        sender_signature: vec![],
    };
    // Sign the canonical tx bytes with the submitter's Ed25519 key (the
    // sentinel's fail-closed sender-auth binds envelope sender == tx.sender).
    let canonical = tx.canonical_signature_bytes().expect("canonical tx bytes");
    tx.sender_signature = submitter_id.ed25519.sign_data(&canonical).expect("sender signs tx");
    tx
}

fn build_committer(
    identity: &Arc<NodeIdentity>,
    env: Arc<EnvironmentMetadata>,
    registry: Arc<NodeRegistry>,
    tokens: Arc<dashmap::DashMap<Vec<u8>, Token>>,
    data_provider: Arc<TestDataProvider>,
) -> Committer {
    let gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Committer,
        role_config(identity),
        60,
        env.asym_crypto_provider.clone(),
    ));

    // Epoch machinery (the committer construction contract — mirrors the
    // S6.1 fixture; none of it runs during a standard commit).
    let stake_store = Arc::new(StakeStore::new());
    let staking_manager =
        Arc::new(StakingManager::new(stake_store.clone(), env.logger.clone()));
    let default_dp = Arc::new(pneumatic_core::data::DefaultDataProvider::new());
    let candidate_registry = Arc::new(CandidateRegistry::new());
    let epoch_reconciler = Arc::new(EpochReconciler::new(
        stake_store.clone(),
        candidate_registry.clone(),
        default_dp,
        "test".to_string(),
        vec![TOKEN_ID.to_vec()],
        env.cost_model.slash_fraction,
    ));
    let hash_provider = Arc::new(BasicHashProvider::new());
    let leader_selector = Arc::new(LeaderSelector::new(hash_provider.clone()));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64;
    let initial_epoch = Epoch::new_with_leader(
        1,
        now,
        now + 300,
        leader_selector.as_ref(),
        &stake_store.to_stake_set(),
        &[],
    );
    let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
    let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));

    let pool = Arc::new(ShieldedPool::new(10));
    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider.clone(),
        registry.clone(),
        env.clone(),
        env.logger.clone(),
        identity.clone(),
        pool.clone(),
    ));

    Committer::new(
        env,
        identity.ed25519.public_key().expect("committer pk"),
        identity.clone(),
        gossiper,
        block_services,
        registry,
        tokens,
        Arc::new(PendingTransactionRegistry::new()),
        stake_store,
        staking_manager,
        epoch_reconciler,
        leader_selector,
        data_provider,
        0,
        Some(epoch_detector),
        block_proposer,
        300,
        5000,
        candidate_registry,
        pool,
    )
}
