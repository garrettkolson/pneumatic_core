//! Stage-0 viability spike: rns-rs (rns-net 0.7.0 / rns-crypto 0.1.9 / rns-core 0.1.16)
//! as the node-to-node transport for pneumatic.
//!
//! Checks (see plan file for context):
//!   1. RnsNode is Send + Sync and shareable behind an Arc (worker-pool model).
//!   2. Two in-process nodes over loopback UDP deliver encrypted messages A->B and B->A.
//!   3. on_announce fires on both sides with matching rhash + public key.
//!   4. A message received on an RNS thread can trigger a send from a spawned
//!      worker thread using an Arc-shared RnsNode (the RegisterAck reply path).
//!   5. Multi-peer topology: a hub with TWO UDP interfaces (transport_enabled=true)
//!      relays data and re-announces, so two leaf nodes that only see the hub
//!      discover each other and talk (multi-hop / transitive discovery).
//!   6. Cold start: can we send to a peer whose (rhash, public key) we know from
//!      config BEFORE receiving its announce? (Informational — record outcome.)
//!   7. Clean shutdown: Arc::try_unwrap(...).shutdown() with no worker refs left.
//!
//! Mirrors the rns-net 0.7.0 e2e UDP test pattern verbatim where possible.
//! NodeConfig has ~45 fields and no Default; this literal is the single place
//! that must be re-migrated on a version bump.

use std::io;
use std::sync::mpsc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rns_crypto::identity::Identity;
use rns_crypto::OsRng;
use rns_net::{
    AnnouncedIdentity, Callbacks, DestHash, Destination, IdentityHash, InterfaceConfig,
    InterfaceId, MODE_FULL, NodeConfig, PacketHash, ProofStrategy, RnsNode, UdpConfig,
};

const APP_NAME: &str = "pneumatic_spike";
const ASPECTS: [&str; 2] = ["udp", "spike"];
const TIMEOUT: Duration = Duration::from_secs(15);
const SETTLE: Duration = Duration::from_millis(1500);
const KNOWN_DESTINATIONS_TTL: Duration = Duration::from_secs(48 * 60 * 60);

// ---------------------------------------------------------------------------
// Event plumbing (mirrors rns-net e2e TestEvent/TestCallbacks)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum SpikeEvent {
    Announce(AnnouncedIdentity),
    Delivery { dest_hash: DestHash, raw: Vec<u8> },
    // Recorded for path-quality observation; not asserted on in this spike.
    #[allow(dead_code)]
    PathUpdated { dest_hash: DestHash, hops: u8 },
}

struct SpikeCallbacks {
    tx: mpsc::Sender<SpikeEvent>,
    /// Set by main after the node starts: when a delivery arrives and this is
    /// Some(cfg), the callback SPAWNS A WORKER THREAD that sends cfg.payload to
    /// cfg.node/cfg.to — proving an Arc-shared RnsNode is usable from a spawned
    /// thread in reply to a received message (the RegisterAck path).
    auto_reply: Arc<Mutex<Option<ReplyConfig>>>,
}

struct ReplyConfig {
    node: Arc<RnsNode>,
    to: Destination,
    payload: Vec<u8>,
}

impl Callbacks for SpikeCallbacks {
    fn on_announce(&mut self, announced: AnnouncedIdentity) {
        let _ = self.tx.send(SpikeEvent::Announce(announced));
    }

    fn on_local_delivery(&mut self, dest_hash: DestHash, raw: Vec<u8>, _packet_hash: PacketHash) {
        // Only the frame-size guard + enqueue happen on the RNS thread.
        // (In production: MAX_FRAME_SIZE check, then bounded channel.)
        let auto_reply = Arc::clone(&self.auto_reply);
        let _ = self.tx.send(SpikeEvent::Delivery { dest_hash, raw });

        let maybe_reply = auto_reply.lock().unwrap().take();
        if let Some(cfg) = maybe_reply {
            let payload = cfg.payload;
            let to = cfg.to.clone();
            let node = Arc::clone(&cfg.node);
            std::thread::spawn(move || {
                let start = Instant::now();
                match node.send_packet(&to, &payload) {
                    Ok(_) => println!(
                        "  [worker-reply] send_packet from spawned thread OK in {:?} ({} bytes)",
                        start.elapsed(),
                        payload.len()
                    ),
                    Err(e) => println!(
                        "  [worker-reply] send_packet from spawned thread FAILED: {e:?}"
                    ),
                }
            });
        }
    }

    fn on_path_updated(&mut self, dest_hash: DestHash, hops: u8) {
        let _ = self.tx.send(SpikeEvent::PathUpdated { dest_hash, hops });
    }
}

// ---------------------------------------------------------------------------
// Helpers (verbatim patterns from rns-net 0.7.0 tests/e2e.rs)
// ---------------------------------------------------------------------------

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

fn decrypt_delivery(raw: &[u8], identity: &Identity) -> Option<Vec<u8>> {
    let packet = rns_core::packet::RawPacket::unpack(raw).ok()?;
    identity.decrypt(&packet.data).ok()
}

fn wait_for_event<F, T>(
    rx: &mpsc::Receiver<SpikeEvent>,
    timeout: Duration,
    mut predicate: F,
) -> Option<T>
where
    F: FnMut(&SpikeEvent) -> Option<T>,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now()).unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok(event) => {
                if let Some(result) = predicate(&event) {
                    return Some(result);
                }
            }
            Err(_) => return None,
        }
    }
}

fn wait_for_announce(
    rx: &mpsc::Receiver<SpikeEvent>,
    expected_hash: &DestHash,
    timeout: Duration,
) -> Option<AnnouncedIdentity> {
    wait_for_event(rx, timeout, |event| match event {
        SpikeEvent::Announce(a) if a.dest_hash == *expected_hash => Some(a.clone()),
        _ => None,
    })
}

fn wait_for_delivery(
    rx: &mpsc::Receiver<SpikeEvent>,
    timeout: Duration,
) -> Option<(DestHash, Vec<u8>)> {
    wait_for_event(rx, timeout, |event| match event {
        SpikeEvent::Delivery { dest_hash, raw } => Some((dest_hash.clone(), raw.clone())),
        _ => None,
    })
}

/// Build the full ~45-field NodeConfig literal. This is the single choke point
/// for rns-net API churn.
///
/// cfg notes:
/// - `backbone_peer_pool: None` is included UNCONDITIONALLY (iface-backbone is a
///   default feature, so the field exists), and the hooks-gated `provider_bridge`
///   field is OMITTED (hooks is non-default).
/// - `transport_enabled` is the parameter: true for relays/hubs, false for the
///   simple leaf pair.
fn build_node_config(
    identity: &Identity,
    transport_enabled: bool,
    ifaces: Vec<(u16, Option<u16>)>, // (listen_port, forward_port)
) -> NodeConfig {
    let interfaces: Vec<InterfaceConfig> = ifaces
        .into_iter()
        .enumerate()
        .map(|(i, (listen_port, forward_port))| {
            let config = UdpConfig {
                name: format!("spike-udp-{i}"),
                listen_ip: Some("127.0.0.1".into()),
                listen_port: Some(listen_port),
                forward_ip: forward_port.map(|_| "127.0.0.1".into()),
                forward_port,
                interface_id: InterfaceId((i + 1) as u64),
                ..UdpConfig::default()
            };
            InterfaceConfig {
                name: String::new(),
                type_name: "UDPInterface".to_string(),
                config_data: Box::new(config),
                mode: MODE_FULL,
                gravity: 0,
                recursive_prs: false,
                announces_from_internal: true,
                announces_to_internal: None,
                ingress_control: rns_core::transport::types::IngressControlConfig::enabled(),
                ifac: None,
                discovery: None,
            }
        })
        .collect();

    NodeConfig {
        panic_on_interface_error: false,
        transport_enabled,
        static_transport_identity: false,
        local_hops_delta: false,
        identity: Some(Identity::from_private_key(
            &identity.get_private_key().unwrap(),
        )),
        interfaces,
        share_instance: false,
        instance_name: "default".into(),
        shared_instance_port: 37428,
        rpc_port: 0,
        cache_dir: None,
        ratchet_store: None,
        ratchet_expiry: Duration::from_secs(rns_core::constants::RATCHET_EXPIRY),
        management: Default::default(),
        probe_port: None,
        probe_addrs: vec![],
        probe_protocol: rns_core::holepunch::ProbeProtocol::Rnsp,
        device: None,
        hooks: Vec::new(),
        discover_interfaces: false,
        autoconnect_interface_mode: None,
        autoconnect_interface_gravity: 0,
        autoconnect_announces_to_internal: false,
        discovery_required_value: None,
        respond_to_probes: false,
        prefer_shorter_path: false,
        max_paths_per_destination: 1,
        packet_hashlist_max_entries: rns_core::constants::HASHLIST_MAXSIZE,
        packet_hashlist_allocation: rns_core::transport::types::PacketHashlistAllocation::Eager,
        max_discovery_pr_tags: rns_core::constants::MAX_PR_TAGS,
        max_path_destinations: usize::MAX,
        max_tunnel_destinations_total: usize::MAX,
        known_destinations_ttl: KNOWN_DESTINATIONS_TTL,
        known_destinations_max_entries: 8192,
        announce_table_ttl: Duration::from_secs(
            rns_core::constants::ANNOUNCE_TABLE_TTL as u64,
        ),
        announce_table_max_bytes: rns_core::constants::ANNOUNCE_TABLE_MAX_BYTES,
        driver_event_queue_capacity: rns_net::event::DEFAULT_EVENT_QUEUE_CAPACITY,
        interface_writer_queue_capacity: rns_net::interface::DEFAULT_ASYNC_WRITER_QUEUE_CAPACITY,
        announce_rate_defaults: rns_net::AnnounceRateDefaults::default(),
        ingress_control_defaults: rns_core::transport::types::IngressControlConfig::enabled(),
        backbone_peer_pool: None,
        announce_sig_cache_enabled: true,
        announce_sig_cache_max_entries: rns_core::constants::ANNOUNCE_SIG_CACHE_MAXSIZE,
        announce_sig_cache_ttl: Duration::from_secs(
            rns_core::constants::ANNOUNCE_SIG_CACHE_TTL as u64,
        ),
        registry: None,
    }
}

struct SpikeNode {
    name: &'static str,
    node: Arc<RnsNode>,
    identity: Identity,
    dest: Destination,
    rx: mpsc::Receiver<SpikeEvent>,
    auto_reply: Arc<Mutex<Option<ReplyConfig>>>,
}

fn start_node(
    name: &'static str,
    identity: Identity,
    transport_enabled: bool,
    ifaces: Vec<(u16, Option<u16>)>,
) -> io::Result<SpikeNode> {
    let ih = IdentityHash(*identity.hash());
    let dest = Destination::single_in(APP_NAME, &ASPECTS, ih)
        .set_proof_strategy(ProofStrategy::ProveAll);
    let (tx, rx) = mpsc::channel();
    let auto_reply: Arc<Mutex<Option<ReplyConfig>>> = Arc::new(Mutex::new(None));
    let config = build_node_config(&identity, transport_enabled, ifaces);
    let node = RnsNode::start(
        config,
        Box::new(SpikeCallbacks {
            tx,
            auto_reply: Arc::clone(&auto_reply),
        }),
    )?;
    let node = Arc::new(node);
    node.register_destination_with_proof(&dest, Some(identity.get_private_key().unwrap()))
        .expect("failed to register destination");
    Ok(SpikeNode {
        name,
        node,
        identity,
        dest,
        rx,
        auto_reply,
    })
}

fn rhash_hex(node: &SpikeNode) -> String {
    hex::encode(node.identity.hash())
}

/// Tear down a node by taking ownership and unwrapping its Arc — proves no
/// lingering worker-thread refs to the RnsNode remain (e.g. from the
/// auto-reply thread in check 4).
fn shutdown_node(node: SpikeNode) {
    let name = node.name;
    let node = match Arc::try_unwrap(node.node) {
        Ok(n) => n,
        Err(_) => panic!("[{name}] Arc<RnsNode> still shared (worker thread?) at shutdown"),
    };
    node.shutdown();
    println!("  [{name}] clean shutdown via Arc::try_unwrap().shutdown()");
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

fn assert_send_sync<T: Send + Sync>() {}

fn main() {
    println!("\n=== Stage-0 RNS spike (rns-net 0.7.0, rns-crypto 0.1.9, rns-core 0.1.16) ===");

    // Check 1: RnsNode: Send + Sync, Arc-shareable.
    assert_send_sync::<RnsNode>();
    println!("\n[check 1] RnsNode: Send + Sync — OK (compile-time)");

    // ------------------------------------------------------------------
    // Topology A: two leaves, point-to-point UDP, transport disabled both.
    // Covers checks 2, 3, 4, 6, 7.
    // ------------------------------------------------------------------
    let (port_alice, port_bob) = (find_free_port(), find_free_port());
    let alice_identity = Identity::new(&mut OsRng);
    let bob_identity = Identity::new(&mut OsRng);

    println!(
        "\n[topology A] alice listen {} -> bob listen {} (P2P UDP, transport off)",
        port_alice, port_bob
    );
    let alice = start_node(
        "alice",
        alice_identity,
        false,
        vec![(port_alice, Some(port_bob))],
    )
    .expect("alice node start");
    let bob = start_node(
        "bob",
        bob_identity,
        false,
        vec![(port_bob, Some(port_alice))],
    )
    .expect("bob node start");

    println!("  alice rhash = {}", rhash_hex(&alice));
    println!("  bob   rhash = {}", rhash_hex(&bob));

    std::thread::sleep(SETTLE); // link establishment

    // Check 6 (informational): try sending BEFORE any announce is seen.
    // Build the outbound destination from KNOWN (rhash, public key) — the
    // cold-start bootstrap case where config carries the peer's identity.
    let synthesized = AnnouncedIdentity {
        dest_hash: DestHash([0u8; 16]), // unused by single_out (recomputed)
        identity_hash: IdentityHash(*bob.identity.hash()),
        public_key: bob.identity.get_public_key().unwrap(),
        app_data: None,
        hops: 0,
        received_at: 0.0,
        receiving_interface: InterfaceId(0),
        rssi: None,
        snr: None,
    };
    let dest_bob_synthetic = Destination::single_out(APP_NAME, &ASPECTS, &synthesized);
    let pre_announce = Instant::now();
    let pre_announce_result = alice.node.send_packet(&dest_bob_synthetic, b"pre-announce?");
    println!(
        "\n[check 6] pre-announce send (known rhash+key, no announce received): {:?}",
        pre_announce_result.as_ref().map_err(|e| e.to_string()).map(|_| "sent")
    );
    // Drain any delivery that might arrive from that attempt (or not).
    let _ = wait_for_delivery(&bob.rx, Duration::from_secs(2));
    let _ = pre_announce;

    // Check 3: both sides announce; both sides receive each other's announce.
    alice
        .node
        .announce(&alice.dest, &alice.identity, Some(b"alice-app-data"))
        .expect("alice announce");
    bob.node
        .announce(&bob.dest, &bob.identity, Some(b"bob-app-data"))
        .expect("bob announce");

    let announced_bob = wait_for_announce(&alice.rx, &bob.dest.hash, TIMEOUT)
        .expect("alice did not receive bob's announce");
    let announced_alice = wait_for_announce(&bob.rx, &alice.dest.hash, TIMEOUT)
        .expect("bob did not receive alice's announce");
    assert_eq!(announced_bob.identity_hash.0, *bob.identity.hash());
    assert_eq!(
        announced_bob.public_key,
        bob.identity.get_public_key().unwrap(),
        "announced public key mismatch"
    );
    assert_eq!(announced_alice.app_data.as_deref(), Some(b"alice-app-data".as_ref() as &[u8]));
    println!(
        "\n[check 3] on_announce both sides — OK (rhash + public key + app_data verified)"
    );

    let dest_bob = Destination::single_out(APP_NAME, &ASPECTS, &announced_bob);
    let dest_alice = Destination::single_out(APP_NAME, &ASPECTS, &announced_alice);

    // Check 2: encrypted delivery A->B and B->A.
    let t0 = Instant::now();
    alice
        .node
        .send_packet(&dest_bob, b"hello-from-alice")
        .expect("alice->bob send");
    let (_, raw) = wait_for_delivery(&bob.rx, TIMEOUT).expect("bob did not receive alice's message");
    let plaintext = decrypt_delivery(&raw, &bob.identity).expect("bob could not decrypt");
    assert_eq!(plaintext, b"hello-from-alice");
    println!(
        "\n[check 2] alice->bob encrypted delivery — OK in {:?} ({} bytes plaintext)",
        t0.elapsed(),
        raw.len()
    );

    let t0 = Instant::now();
    bob.node
        .send_packet(&dest_alice, b"hello-from-bob")
        .expect("bob->alice send");
    let (_, raw) = wait_for_delivery(&alice.rx, TIMEOUT).expect("alice did not receive bob's message");
    let plaintext = decrypt_delivery(&raw, &alice.identity).expect("alice could not decrypt");
    assert_eq!(plaintext, b"hello-from-bob");
    println!(
        "       bob->alice encrypted delivery — OK in {:?} ({} bytes plaintext)",
        t0.elapsed(),
        raw.len()
    );

    // Check 4: worker-thread reply (RegisterAck path).
    // Bob's callback, on the next delivery, spawns a thread that sends a reply
    // using an Arc<RnsNode> clone. That is exactly the production shape:
    // RNS thread enqueues, worker pool thread verifies + responds.
    *bob.auto_reply.lock().unwrap() = Some(ReplyConfig {
        node: Arc::clone(&bob.node),
        to: dest_alice.clone(),
        payload: b"pong-from-bob-worker-thread".to_vec(),
    });
    let t0 = Instant::now();
    alice.node
        .send_packet(&dest_bob, b"ping")
        .expect("alice ping");
    let (_, raw) = wait_for_delivery(&alice.rx, TIMEOUT).expect("alice did not receive worker reply");
    let plaintext = decrypt_delivery(&raw, &alice.identity).expect("alice could not decrypt reply");
    assert_eq!(plaintext, b"pong-from-bob-worker-thread");
    println!(
        "\n[check 4] worker-thread reply via Arc<RnsNode> — OK (full round-trip in {:?})",
        t0.elapsed()
    );

    // Check 7: clean shutdown (after the worker has released its Arc clone).
    std::thread::sleep(SETTLE / 2);
    println!("\n[check 7] clean shutdown:");
    shutdown_node(bob);
    shutdown_node(alice);

    // ------------------------------------------------------------------
    // Topology B: hub with two UDP interfaces (transport on) + two leaves
    // (transport off). Covers check 5: relay + transitive announce.
    // ------------------------------------------------------------------
    let (port_hub1, port_hub2, port_leaf_a, port_leaf_c) =
        (find_free_port(), find_free_port(), find_free_port(), find_free_port());
    let hub_identity = Identity::new(&mut OsRng);
    let leaf_a_identity = Identity::new(&mut OsRng);
    let leaf_c_identity = Identity::new(&mut OsRng);

    println!(
        "\n[topology B] leaf_a<->hub<->leaf_c (hub transport ON, leaves OFF) — multi-hop"
    );
    let hub = start_node(
        "hub",
        hub_identity,
        true,
        vec![(port_hub1, Some(port_leaf_a)), (port_hub2, Some(port_leaf_c))],
    )
    .expect("hub node start");
    let leaf_a = start_node(
        "leaf_a",
        leaf_a_identity,
        false,
        vec![(port_leaf_a, Some(port_hub1))],
    )
    .expect("leaf_a node start");
    let leaf_c = start_node(
        "leaf_c",
        leaf_c_identity,
        false,
        vec![(port_leaf_c, Some(port_hub2))],
    )
    .expect("leaf_c node start");

    std::thread::sleep(SETTLE);

    hub.node
        .announce(&hub.dest, &hub.identity, Some(b"hub"))
        .expect("hub announce");
    leaf_a
        .node
        .announce(&leaf_a.dest, &leaf_a.identity, Some(b"leaf-a"))
        .expect("leaf_a announce");
    leaf_c
        .node
        .announce(&leaf_c.dest, &leaf_c.identity, Some(b"leaf-c"))
        .expect("leaf_c announce");

    // Transitive discovery: each leaf learns the OTHER leaf via the hub.
    let announced_leaf_a = wait_for_announce(&leaf_c.rx, &leaf_a.dest.hash, TIMEOUT)
        .expect("leaf_c did not learn leaf_a via hub (relay/announce propagation failed)");
    let announced_leaf_c = wait_for_announce(&leaf_a.rx, &leaf_c.dest.hash, TIMEOUT)
        .expect("leaf_a did not learn leaf_c via hub (relay/announce propagation failed)");
    assert_eq!(
        announced_leaf_a.identity_hash.0,
        *leaf_a.identity.hash()
    );
    assert_eq!(
        announced_leaf_c.identity_hash.0,
        *leaf_c.identity.hash()
    );
    println!("\n[check 5a] transitive announce via hub — OK (leaves discovered each other)");

    // Multi-hop data: leaf_a -> hub (relay) -> leaf_c.
    let dest_leaf_c = Destination::single_out(APP_NAME, &ASPECTS, &announced_leaf_c);
    let t0 = Instant::now();
    leaf_a
        .node
        .send_packet(&dest_leaf_c, b"cross-hub-message")
        .expect("leaf_a->leaf_c send");
    let (_, raw) = wait_for_delivery(&leaf_c.rx, TIMEOUT)
        .expect("leaf_c did not receive leaf_a's message via hub");
    let plaintext = decrypt_delivery(&raw, &leaf_c.identity).expect("leaf_c could not decrypt");
    assert_eq!(plaintext, b"cross-hub-message");
    println!(
        "       multi-hop data leaf_a->hub->leaf_c — OK in {:?} ({} bytes on wire)",
        t0.elapsed(),
        raw.len()
    );

    println!("\n[check 5b] hub relay with transport_enabled=true — OK");
    std::thread::sleep(SETTLE / 2);
    println!("\n[check 7] clean shutdown (topology B):");
    shutdown_node(leaf_a);
    shutdown_node(leaf_c);
    shutdown_node(hub);

    println!("\n=== ALL SPIKE CHECKS PASSED ===");
    println!("Findings recorded in plan file. Safe to proceed to Stage 1.");
}
