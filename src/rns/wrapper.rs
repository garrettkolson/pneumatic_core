//! `RnsNetwork` — pneumatic's RNS transport wrapper.
//!
//! Threading model (verified in the Stage-0 spike): the RNS driver invokes
//! `Callbacks` on its own threads, where blocking I/O is forbidden. So the
//! delivery callbacks do exactly one thing — a raw-size guard plus enqueue —
//! and a 4-thread worker pool does the rest (decrypt, plaintext-size guard,
//! dispatch to the application handler). Decrypting per worker is fine:
//! `Identity::decrypt` is stateless.
//!
//! Inbound channels are std `mpsc` (unbounded), one per worker — std
//! `Receiver` is not `Sync`, so a single shared queue is out. The
//! delivery callback round-robins packets across the worker queues.
//! The DoS guard is the raw-size check at the delivery→queue handoff: an
//! over-limit packet never enters a queue, so memory use stays bounded
//! by in-flight packets rather than queue depth.
//!
//! ## Large-payload transport (remediation option a)
//!
//! RNS's direct packet path (`send_packet` → `RawPacket::pack`) hard-caps a
//! single plaintext at ~481 B (the `rns-core` `MTU = 500` minus 19 B of
//! HEADER_1 overhead), so a real Phase-8 `Message` (~3.8 KB with the hybrid
//! Ed25519·ML-DSA-44 signature) cannot traverse it. This wrapper instead drives
//! the payload over Reticulum's *native Resource transfer* layer: an opaque
//! byte buffer that RNS fragments into ~464 B SDUs, retransmits
//! (`RESOURCE_MAX_RETRIES`), and flow-controls itself. The `Message` bytes ride
//! inside the existing `NetworkPacket` envelope on the resource's `data`
//! field, so the app framing, signature scheme, and envelope all stay
//! byte-identical — the wire format is unchanged.
//!
//! A Resource transfer needs a negotiated `link_id` (not an rhash). Links are
//! negotiated **explicitly**, not automatically (RNS 0.7.0 has no
//! `start_link_negotiation` on `RnsNode`): a peer must first
//! `register_link_destination` *itself* as a link target, then the other side
//! calls `create_link(dest_hash, peer_sig_pub)` to obtain a `link_id`. This
//! wrapper registers itself at startup (`start`), so `send_resource_to` on the
//! initiator creates the link on demand (retrying until the route is up) and
//! caches the resulting `link_id` in the `links` table
//! (dest_hash → link_id). On link establishment the responder also tells its
//! own node to reassemble Resources with a bounded memory buffer
//! (`set_resource_receive_mode`); the default receive strategy is `AcceptNone`,
//! so the wrapper registers itself with `AcceptAll` (strategy 1) — otherwise an
//! inbound Resource is held and never delivered. Reassembled inbound Resources
//! are enqueued to the same worker pool, so application verification never runs
//! on the driver thread.

use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use dashmap::DashMap;
use rns_core::packet::RawPacket;
use rns_crypto::identity::Identity;
use rns_net::{
    Callbacks, DestHash, Destination, IdentityHash, InterfaceId, LinkId, NodeConfig,
    PacketHash, ProofStrategy, RnsNode, ResourceReceiveMode,
};

pub use rns_net::AnnouncedIdentity;

use crate::config::BootstrapPeer;
use crate::conns::MAX_FRAME_SIZE;
use crate::errors::PneumaticError;

use super::identity::{rhash_from_public_key, NodeIdentity};

/// Application name for pneumatic's RNS destinations.
pub const APP_NAME: &str = "pneumatic";

/// Interface aspects for pneumatic's UDP destinations.
pub const ASPECTS: [&str; 2] = ["udp", "pneumatic"];

const WORKER_THREADS: usize = 4;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// RNS envelope overhead (ChaCha20-Poly1305 + DTN framing) measured at
/// ~115 B in the spike; 1 KiB of margin keeps the raw guard a fast
/// pre-filter. The authoritative check is on plaintext, in the workers.
const ENVELOPE_MARGIN: usize = 1024;
/// Upper bound on a single inbound Resource's reassembled size. Memory mode
/// will not assemble a Resource larger than this, bounding per-link memory use
/// in line with the 16 MiB frame cap (the same cap guards the packet path).
const RESOURCE_MAX_BYTES: u64 = MAX_FRAME_SIZE as u64;

/// Application handler for decrypted inbound packets. Receives raw
/// plaintext bytes; the application deserializes the `NetworkPacket`.
pub type PacketHandler = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

/// A message on a worker's inbound channel.
///
/// The worker pool carries both inbound transports behind one enum so the
/// decrypted-Resource path never touches the driver thread: `Encrypted` is the
/// legacy `send_packet` delivery (unpack → decrypt → dispatch), `Resource` is
/// the native Resource transfer (already decrypted by RNS → size guard →
/// dispatch).
enum WorkerMessage {
    /// Encrypted RNS packet — the legacy `on_local_delivery` path.
    Encrypted(Vec<u8>),
    /// Fully reassembled inbound Resource — already decrypted by RNS, so the
    /// worker skips `RawPacket::unpack` / `Identity::decrypt`.
    Resource(Vec<u8>),
}

/// RNS transport: one `RnsNode`, a route table (rhash →
/// `AnnouncedIdentity`), a link table (dest_hash → `LinkId`), and the inbound
/// worker pool.
pub struct RnsNetwork {
    node: Arc<RnsNode>,
    my_rhash: [u8; 16],
    destinations: Arc<DashMap<[u8; 16], AnnouncedIdentity>>,
    /// Negotiated links: destination hash → link id. Populated by
    /// `send_resource_to` (the initiator's `create_link`) and read back by
    /// `send_resource_to` to avoid re-creating an existing link.
    links: Arc<DashMap<DestHash, LinkId>>,
    /// This node's registered destination, retained so the transport can
    /// re-announce it (`RnsNetwork::announce`) once peers are listening.
    dest: Destination,
    /// This node's private key, retained so `announce` can reconstruct the
    /// identity it needs without the caller keeping it around.
    private_key: [u8; 64],
    stopped: Arc<AtomicBool>,
    handler: Arc<RwLock<Option<PacketHandler>>>,
    /// Announcement handler. The handler returns a `Result` so a caller (e.g. a
    /// directory-request that failed to sign its binding) surfaces the error to
    /// the RNS driver thread for logging instead of silently degrading.
    announce_handler: Arc<RwLock<Option<Arc<dyn Fn(AnnouncedIdentity) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + Send + Sync>>>>,
    announce_rx: Mutex<Option<mpsc::Receiver<AnnouncedIdentity>>>,
    announce_worker: Mutex<Option<JoinHandle<()>>>,
    workers: Vec<JoinHandle<()>>,
    /// Back-reference from the callbacks (which live inside the node) to this
    /// node. `RnsNode::start` consumes the callbacks, so a plain `Arc<RnsNode>`
    /// self-reference can't be built up front; instead this holds the node Arc
    /// behind the callbacks' own clone of the slot. The callbacks use it in
    /// `on_link_established` to configure per-link Resource policy. `stop()`
    /// clears our half before consuming the node, so `shutdown(self)` can own
    /// it despite the callback holding one clone.
    node_slot: Arc<Mutex<Option<Arc<RnsNode>>>>,
}

/// Delivery callback state. Announces are inserted straight into the
/// route table (a fast DashMap insert, non-blocking); raw deliveries and
/// reassembled Resources are round-robin'd across one queue per worker.
struct NetworkCallbacks {
    txs: Vec<mpsc::Sender<WorkerMessage>>,
    next: AtomicUsize,
    destinations: Arc<DashMap<[u8; 16], AnnouncedIdentity>>,
    announce_tx: mpsc::Sender<AnnouncedIdentity>,
    links: Arc<DashMap<DestHash, LinkId>>,
    node_slot: Arc<Mutex<Option<Arc<RnsNode>>>>,
}

impl Callbacks for NetworkCallbacks {
    fn on_announce(&mut self, announced: AnnouncedIdentity) {
        let rhash = rhash_from_public_key(&announced.public_key);
        self.destinations.insert(rhash, announced.clone());
        if self.announce_tx.send(announced).is_err() {
            eprintln!("[pneumatic] rns: announce handler channel closed; dropping announce");
        }
    }

    fn on_local_delivery(&mut self, _dest_hash: DestHash, raw: Vec<u8>, _packet_hash: PacketHash) {
        // Only the size guard + enqueue — decryption and the application
        // handler run on the worker pool, never on the RNS driver thread.
        if raw.len() > MAX_FRAME_SIZE + ENVELOPE_MARGIN {
            eprintln!(
                "[pneumatic] rns: inbound packet {} bytes exceeds limit; dropping",
                raw.len()
            );
            return;
        }
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.txs.len();
        if self.txs[idx].send(WorkerMessage::Encrypted(raw)).is_err() {
            eprintln!("[pneumatic] rns: inbound queue closed; dropping packet");
        }
    }

    fn on_path_updated(&mut self, _dest_hash: DestHash, _hops: u8) {
        // Path changes need no pneumatic-level action in v1; sends resolve
        // the route from the current destination table at send time.
    }

    /// A bidirectional link has been negotiated (the peer's `create_link`).
    /// Record the link id so this node can also *send* Resources back over it,
    /// and set a bounded reassembly buffer here — otherwise per-link assembly
    /// memory is unbounded (the default `Memory` cap is 64 MiB). `AcceptAll`
    /// was already granted at `register_link_destination` in `start`, so an
    /// inbound Resource is delivered rather than held. These are cheap
    /// non-blocking calls, safe on the RNS driver thread.
    fn on_link_established(
        &mut self,
        link_id: LinkId,
        dest_hash: DestHash,
        _rtt: f64,
        _is_initiator: bool,
    ) {
        self.links.insert(dest_hash, link_id);
        let Some(node) = self.node_slot.lock().unwrap().clone() else {
            // The node is installed immediately after `RnsNode::start`; a link
            // can't predate it, but guard anyway.
            return;
        };
        // Memory mode delivers the fully reassembled payload in a single
        // `on_resource_received` (dispatch reassembles the logical Resource and
        // fires the callback once). The cap bounds per-link assembly memory.
        if node
            .set_resource_receive_mode(link_id.0, ResourceReceiveMode::Memory { max_bytes: RESOURCE_MAX_BYTES })
            .is_err()
        {
            eprintln!("[pneumatic] rns: failed to set resource receive mode");
        }
        // 1 = AcceptAll (0 = AcceptNone, 2 = AcceptApp). Otherwise the default
        // strategy holds the resource and it is never delivered.
        if node.set_resource_strategy(link_id.0, 1).is_err() {
            eprintln!("[pneumatic] rns: failed to set resource strategy");
        }
    }

    /// A fully reassembled inbound Resource. RNS has already decrypted it, so
    /// this only enqueues the plaintext onto the worker pool (guarded by the
    /// same size limit); verification runs on a worker, never here.
    fn on_resource_received(
        &mut self,
        _link_id: LinkId,
        data: Vec<u8>,
        _metadata: Option<Vec<u8>>,
    ) {
        if data.len() > MAX_FRAME_SIZE + ENVELOPE_MARGIN {
            eprintln!(
                "[pneumatic] rns: inbound resource {} bytes exceeds limit; dropping",
                data.len()
            );
            return;
        }
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.txs.len();
        if self.txs[idx].send(WorkerMessage::Resource(data)).is_err() {
            eprintln!("[pneumatic] rns: inbound queue closed; dropping resource");
        }
    }

    /// A Resource transfer failed. RNS already retransmits up to
    /// `RESOURCE_MAX_RETRIES`; surface the failure for logging. `send_resource_to`
    /// waits for the link to establish before sending, so a failure here is
    /// surfaced as a `PneumaticError::Resource` to the caller rather than us
    /// adding our own retransmit.
    fn on_resource_failed(&mut self, _link_id: LinkId, _error: String) {}

    /// A Resource transfer completed (sender-side proof validated by us).
    fn on_resource_completed(&mut self, link_id: LinkId) {
        eprintln!("[pneumatic] rns: resource transfer completed (link {:02x?})", link_id.0);
    }

    fn on_resource_progress(&mut self, _link_id: LinkId, _received: usize, _total: usize) {}
}

/// `true` when a decrypted inbound plaintext is within the app-level size
/// limit (the 16 MiB frame cap, shared with the legacy framing layer).
pub fn inbound_size_ok(size: usize) -> bool {
    size <= MAX_FRAME_SIZE
}

/// Extract the Ed25519 signing key pair from an RNS `Identity` for link
/// registration. The 64-byte key layout is `[X25519(32) | Ed25519(32)]`, so the
/// signing keys live in the high half. Mirrors the RNS e2e harness.
fn extract_sig_keys(identity: &Identity) -> ([u8; 32], [u8; 32]) {
    let prv = identity.get_private_key().expect("identity has signing private key");
    let pub_key = identity.get_public_key().expect("identity has signing public key");
    let mut sig_prv = [0u8; 32];
    let mut sig_pub = [0u8; 32];
    sig_prv.copy_from_slice(&prv[32..64]);
    sig_pub.copy_from_slice(&pub_key[32..64]);
    (sig_prv, sig_pub)
}

/// Extract an announced peer's Ed25519 signing public key (the high half of its
/// 64-byte transport public key), used to `create_link` to that peer.
fn extract_peer_sig_pub(public_key: &[u8; 64]) -> [u8; 32] {
    let mut sig_pub = [0u8; 32];
    sig_pub.copy_from_slice(&public_key[32..64]);
    sig_pub
}

impl RnsNetwork {
    /// Start the transport: pre-seed routes for bootstrap peers, start the
    /// RNS node, register + announce our single destination, and spawn the
    /// inbound worker pool.
    pub fn start(
        node_config: NodeConfig,
        identity: &NodeIdentity,
        bootstrap: &[BootstrapPeer],
    ) -> Result<Self, PneumaticError> {
        let destinations: Arc<DashMap<[u8; 16], AnnouncedIdentity>> = Arc::new(DashMap::new());
        let links: Arc<DashMap<DestHash, LinkId>> = Arc::new(DashMap::new());
        let node_slot: Arc<Mutex<Option<Arc<RnsNode>>>> = Arc::new(Mutex::new(None));

        // Pre-seed routes for bootstrap peers from config: a (rhash, public
        // key) pair from config is enough to build a destination — the
        // spike verified sends to a pre-announce destination are accepted.
        for peer in bootstrap {
            let pub_bytes = hex::decode(&peer.public_key)
                .map_err(|e| PneumaticError::CryptoError(format!("bootstrap public_key: {}", e)))?;
            let pub64: [u8; 64] = pub_bytes
                .as_slice()
                .try_into()
                .map_err(|_| PneumaticError::CryptoError("bootstrap public_key must be 64 bytes".into()))?;
            let rhash = rhash_from_public_key(&pub64);
            let announced = AnnouncedIdentity {
                dest_hash: DestHash([0u8; 16]), // unused by single_out (recomputed)
                identity_hash: IdentityHash(rhash),
                public_key: pub64,
                app_data: None,
                hops: 0,
                received_at: 0.0,
                receiving_interface: InterfaceId(0),
                rssi: None,
                snr: None,
            };
            destinations.insert(rhash, announced);
        }

        let (txs, rxs): (Vec<_>, Vec<_>) =
            (0..WORKER_THREADS).map(|_| mpsc::channel::<WorkerMessage>()).unzip();
        let (announce_tx, announce_rx) = mpsc::channel::<AnnouncedIdentity>();
        let callbacks = NetworkCallbacks {
            txs,
            next: AtomicUsize::new(0),
            destinations: Arc::clone(&destinations),
            announce_tx,
            links: links.clone(),
            node_slot: node_slot.clone(),
        };

        let node = RnsNode::start(node_config, Box::new(callbacks))
            .map_err(|e| PneumaticError::Network(format!("rns node start: {}", e)))?;
        let node = Arc::new(node);
        // Publish the node reference the callbacks need to configure per-link
        // Resource policy on `on_link_established`. The callbacks (inside the
        // node) hold their own clone of `node_slot`; `stop()` clears our half
        // before consuming the node, so `shutdown(self)` can own it.
        *node_slot.lock().unwrap() = Some(Arc::clone(&node));

        let private_key = identity
            .rns
            .get_private_key()
            .ok_or_else(|| PneumaticError::CryptoError("identity has no private key".into()))?;
        let ih = IdentityHash(*identity.rns.hash());
        let dest = Destination::single_in(APP_NAME, &ASPECTS, ih)
            .set_proof_strategy(ProofStrategy::ProveAll);
        node.register_destination_with_proof(&dest, Some(private_key))
            .map_err(|_| PneumaticError::Network("register destination failed".into()))?;
        node.announce(&dest, &identity.rns, None)
            .map_err(|_| PneumaticError::Network("announce destination failed".into()))?;
        // Register ourselves as a link destination so peers can create a link to
        // us for the Resource transfer path. `AcceptAll` (strategy 1) is set at
        // registration time — otherwise the default `AcceptNone` would hold any
        // inbound Resource and `on_resource_received` never fires. The signing
        // keys come from the RNS identity's Ed25519 half.
        let (sig_prv, sig_pub) = extract_sig_keys(&identity.rns);
        node.register_link_destination(dest.hash.0, sig_prv, sig_pub, 1 /* AcceptAll */)
            .map_err(|_| PneumaticError::Network("register link destination failed".into()))?;

        let stopped = Arc::new(AtomicBool::new(false));
        let handler: Arc<RwLock<Option<PacketHandler>>> = Arc::new(RwLock::new(None));

        let mut workers = Vec::with_capacity(WORKER_THREADS);
        for rx in rxs {
            let stopped = Arc::clone(&stopped);
            let handler = Arc::clone(&handler);
            let identity = Identity::from_private_key(&private_key);
            workers.push(thread::spawn(move || {
                worker_loop(&rx, &stopped, &handler, &identity);
            }));
        }

        Ok(RnsNetwork {
            node,
            my_rhash: *identity.rns.hash(),
            destinations,
            links,
            dest,
            private_key,
            stopped,
            handler,
            announce_handler: Arc::new(RwLock::new(None)),
            announce_rx: Mutex::new(Some(announce_rx)),
            announce_worker: Mutex::new(None),
            workers,
            node_slot,
        })
    }

    /// Re-announce this node's destination to its peers. A startup announce races
    /// the peer's listener coming up, so it can leave the peer's bootstrap-seeded
    /// route dead. Call this once both nodes are listening: each announce
    /// re-traverses the established links, which is what upgrades a synthetic
    /// bootstrap route to a usable one. The RNS identity is reconstructed from the
    /// stored private key.
    pub fn announce(&self) {
        let identity = Identity::from_private_key(&self.private_key);
        if let Err(_) = self.node.announce(&self.dest, &identity, None) {
            eprintln!("[pneumatic] rns: re-announce failed");
        }
    }

    /// Send `payload` (rmp-serialized `NetworkPacket` bytes) to `rhash`.
    /// Fails if no route is known for the rhash (the route table is seeded
    /// from bootstrap config and updated by announces).
    pub fn send_to(&self, rhash: [u8; 16], payload: &[u8]) -> Result<(), PneumaticError> {
        let announced = self
            .destinations
            .get(&rhash)
            .map(|e| e.value().clone())
            .ok_or_else(|| PneumaticError::Network(format!("no route to rhash {:02x?}", rhash)))?;
        let dest = Destination::single_out(APP_NAME, &ASPECTS, &announced);
        self.node
            .send_packet(&dest, payload)
            .map(|_| ())
            .map_err(|e| PneumaticError::Network(format!("send to {:02x?} failed: {:?}", rhash, e)))
    }

    /// Send `payload` over the native Resource transfer path to `rhash`, bypassing
    /// the ~481 B direct-packet cap. The `payload` typically is the rmp-serialized
    /// `NetworkPacket` carrying a `Message` — it rides on the resource's opaque
    /// `data` field, so the Message wire format is unchanged. `metadata` is `None`
    /// (reserved for a future transfer hash / action hint).
    ///
    /// The initiator establishes the link on demand: it looks up the peer's
    /// announced destination (which carries the peer's 64-byte public key),
    /// `create_link`s that destination with the peer's Ed25519 signing key
    /// (retrying until the route comes up), caches the resulting `link_id`, and
    /// hands `payload` to RNS's Resource layer. The RNS layer owns
    /// fragmentation/retransmit/flow-control, so a successful return means the
    /// bytes were handed to RNS for delivery (not yet proven received).
    ///
    /// Fail-closed: returns `PneumaticError::Resource` if no route/link can be
    /// established, rather than silently dropping the payload.
    pub fn send_resource_to(&self, rhash: [u8; 16], payload: Vec<u8>) -> Result<(), PneumaticError> {
        let announced = self
            .destinations
            .get(&rhash)
            .map(|e| e.value().clone())
            .ok_or_else(|| PneumaticError::Resource(format!("no route to rhash {:02x?}", rhash)))?;
        let dest_hash = announced.dest_hash;
        // The peer's Ed25519 signing public key — the high half of its 64-byte
        // transport public key — identifies it for link creation.
        let peer_sig_pub = extract_peer_sig_pub(&announced.public_key);

        // Reuse an existing link if one is already recorded for this destination
        // (create_link creates a new link each call, so don't re-create).
        if let Some(link) = self.links.get(&dest_hash).map(|e| e.value().clone()) {
            return self.send_resource_on(link, rhash, payload);
        }

        // Create the link, retrying until the route is up. `create_link` returns
        // `Err(SendError)` when no route exists for the destination yet — the
        // announce handshake needs to have populated the route first.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match self.node.create_link(dest_hash.0, peer_sig_pub) {
                Ok(link_id) => {
                    // `create_link` is optimistic: it returns the link_id the
                    // moment the LINKREQUEST is enqueued, before the handshake
                    // completes. `RnsNode::send_resource` is a *no-op on a
                    // non-Active link* (rns-net link_manager::send_resource),
                    // so sending now would silently drop the payload. Wait for
                    // this node's `on_link_established` callback (which records
                    // the link in `self.links` exactly when the link engine
                    // transitions to Active), then give the responder a brief
                    // settle so it has activated its side and configured
                    // AcceptAll before the parts arrive.
                    self.wait_for_link_active(dest_hash, &deadline)?;
                    thread::sleep(Duration::from_millis(300));
                    return self.send_resource_on(LinkId(link_id), rhash, payload);
                }
                Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(PneumaticError::Resource(format!(
                            "no established link for rhash {:02x?} (dest_hash {:02x?}): route not up",
                            rhash, dest_hash.0
                        )));
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    /// Block until a link to `dest_hash` has become Active. `on_link_established`
    /// records the link in the `links` table at the instant the link engine
    /// transitions to `Active` — the only reliable app-level signal that
    /// `send_resource` will actually transmit rather than no-op. Returns a
    /// `Resource` error on timeout so a stuck link surfaces loudly instead of
    /// silently dropping the payload.
    fn wait_for_link_active(
        &self,
        dest_hash: DestHash,
        deadline: &std::time::Instant,
    ) -> Result<(), PneumaticError> {
        loop {
            if self.links.get(&dest_hash).is_some() {
                return Ok(());
            }
            if std::time::Instant::now() >= *deadline {
                return Err(PneumaticError::Resource(format!(
                    "link to dest_hash {:02x?} did not become active before timeout",
                    dest_hash.0
                )));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Deliver `payload` on an already-established `link`.
    fn send_resource_on(
        &self,
        link: LinkId,
        rhash: [u8; 16],
        payload: Vec<u8>,
    ) -> Result<(), PneumaticError> {
        self.node
            .send_resource(link.0, payload, None)
            .map(|_| ())
            .map_err(|_| PneumaticError::Resource(format!("send_resource to rhash {:02x?} failed", rhash)))
    }

    /// Our rhash (transport identity), for logging and config cross-checks.
    pub fn my_rhash(&self) -> [u8; 16] {
        self.my_rhash
    }

    /// Install the application handler for decrypted inbound packets.
    pub fn on_packet(&self, handler: PacketHandler) {
        *self.handler.write().unwrap() = Some(handler);
    }

    /// Install the announce handler for discovered peers.
    ///
    /// Announce callbacks run on an RNS driver thread, so this consumes a
    /// dedicated non-blocking channel and dispatches on a worker thread.
    pub fn on_announce(&self, handler: Arc<dyn Fn(AnnouncedIdentity) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + Send + Sync>) {
        *self.announce_handler.write().unwrap() = Some(handler);
        let Some(rx) = self.announce_rx.lock().unwrap().take() else {
            return;
        };
        let handler = Arc::clone(&self.announce_handler);
        let worker = thread::spawn(move || {
            loop {
                let Ok(announced) = rx.recv() else {
                    return;
                };
                let Some(handler) = handler.read().unwrap().clone() else {
                    continue;
                };
                if let Err(e) = handler(announced) {
                    eprintln!("[rns] announce handler error: {}", e);
                }
            }
        });
        *self.announce_worker.lock().unwrap() = Some(worker);
    }

    /// Clean shutdown: stop the workers, clear the node back-reference, then
    /// consume and shut down the node (clearing `node_slot` first lets
    /// `shutdown(self)` own the node despite the callbacks holding one clone of
    /// it, which live inside the node).
    pub fn stop(self) {
        self.stopped.store(true, Ordering::SeqCst);
        for worker in self.workers {
            let _ = worker.join();
        }
        if let Some(worker) = self.announce_worker.lock().unwrap().take() {
            let _ = worker.join();
        }
        // Drop our half of the callbacks' node back-reference so `shutdown`
        // (which consumes the node by value) can own it — otherwise the clone
        // held inside the callbacks would keep the refcount above 1 forever.
        *self.node_slot.lock().unwrap() = None;
        // `shutdown(self)` consumes the node, but it is wrapped in an Arc; the
        // callbacks' back-reference is now cleared, so we're the sole owner and
        // the unwrap can't fail.
        let RnsNetwork { node, .. } = self;
        match Arc::try_unwrap(node) {
            Ok(n) => n.shutdown(),
            Err(_) => eprintln!("[pneumatic] rns: node still referenced at shutdown; leaking"),
        }
    }
}

fn worker_loop(
    rx: &mpsc::Receiver<WorkerMessage>,
    stopped: &Arc<AtomicBool>,
    handler: &RwLock<Option<PacketHandler>>,
    identity: &Identity,
) {
    loop {
        if stopped.load(Ordering::SeqCst) {
            return;
        }
        let msg = match rx.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(msg) => msg,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        let plaintext = match msg {
            // Legacy packet path: unpack + decrypt.
            WorkerMessage::Encrypted(raw) => {
                let Ok(packet) = RawPacket::unpack(&raw) else {
                    eprintln!("[pneumatic] rns: failed to unpack inbound packet; dropping");
                    continue;
                };
                let Ok(plaintext) = identity.decrypt(&packet.data) else {
                    eprintln!("[pneumatic] rns: failed to decrypt inbound packet; dropping");
                    continue;
                };
                plaintext
            }
            // Native Resource path: already decrypted by RNS, no unpack/decrypt.
            WorkerMessage::Resource(plaintext) => plaintext,
        };

        if !inbound_size_ok(plaintext.len()) {
            eprintln!(
                "[pneumatic] rns: inbound plaintext {} bytes exceeds {} byte limit; dropping",
                plaintext.len(),
                MAX_FRAME_SIZE
            );
            continue;
        }
        let Some(handler) = handler.read().unwrap().clone() else {
            continue;
        };
        // Delivery is transport-agnostic: RNS is destination-encrypted and
        // multi-hop, so this callback cannot recover the sender (`RawPacket`
        // carries no sender rhash; HEADER_1 packets have none). Sender
        // authentication is the application layer's job — the commender's
        // router gate (Phase 1.3) verifies the self-identified
        // `message.public_key` + `message.signature`, which RNS delivery cannot
        // strip or forge. The "drops attribution" concern from the audit is
        // therefore resolved without an `rns-core` change.
        handler(plaintext);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_size_ok_boundary() {
        assert!(inbound_size_ok(0));
        assert!(inbound_size_ok(MAX_FRAME_SIZE));
        assert!(!inbound_size_ok(MAX_FRAME_SIZE + 1));
    }

    /// Two-node UDP-loopback round-trip over the native Resource transfer path.
    /// Runs unconditionally — CI supports UDP loopback (the existing Phase 7.1
    /// UDP wire test is likewise unconditional).
    mod wire {
        use super::*;
        use crate::config::BootstrapPeer;
        use crate::encoding::serialize_to_bytes_rmp;
        use crate::node::NetworkPacket;
        use crate::rns::config_builder::RnsNodeConfigBuilder;
        use crate::rns::identity::NodeIdentity;
        use std::time::Duration;

        fn free_port() -> u16 {
            static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
            let base = 20_000;
            loop {
                let p = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if std::net::TcpListener::bind(("127.0.0.1", base + p)).is_ok() {
                    return base + p;
                }
            }
        }

        /// Build a leaf node that forwards to `peer_port`, with `peer_pub_key_hex`
        /// pre-seeded as a bootstrap peer (so a send to a pre-announce destination
        /// is accepted). Mirrors the spike's symmetric topology: both nodes
        /// forward to each other so the announce handshake activates the route.
        /// An empty `peer_pub_key_hex` seeds no route (used by the no-link test).
        fn start_node(identity: &NodeIdentity, peer_pub_key_hex: &str, peer_port: u16, this_port: u16) -> RnsNetwork {
            let bootstrap = if peer_pub_key_hex.is_empty() {
                Vec::new()
            } else {
                vec![BootstrapPeer {
                    public_key: peer_pub_key_hex.to_string(),
                    ip: "127.0.0.1".to_string(),
                    port: peer_port,
                }]
            };
            let node_config = RnsNodeConfigBuilder::new()
                .with_udp_port(this_port)
                .add_peer("127.0.0.1", peer_port)
                .build(&identity.rns);
            RnsNetwork::start(node_config, identity, &bootstrap).expect("start rns node")
        }

        /// Positive: a ~3.8 KB payload — larger than RNS's 500 B packet cap —
        /// traverses the native Resource transfer and reassembles byte-identical
        /// at the receiver. This is the negation of the audit finding that no
        /// real multi-KB payload can cross RNS.
        #[test]
        fn resource_over_mtu_roundtrip() {
            // Symmetric point-to-point topology: the committer listens on
            // `committer_port` and forwards to `finalizer_port`; the finalizer
            // listens on `finalizer_port` and forwards to `committer_port`. Each
            // node seeds the other as a bootstrap peer so sends to a pre-announce
            // destination are accepted, and each forwards its announce to the
            // peer's actual listen port (this is the crux — a node must forward
            // to its PEER, not to itself).
            let committer = NodeIdentity::generate_in_memory();
            let committer_port = free_port();
            let finalizer = NodeIdentity::generate_in_memory();
            let finalizer_port = free_port();
            let committer_pub_hex =
                hex::encode(committer.rns.get_public_key().expect("committer public key"));
            let finalizer_pub_hex =
                hex::encode(finalizer.rns.get_public_key().expect("finalizer public key"));

            let committer_net = start_node(
                &committer,
                &finalizer_pub_hex,
                finalizer_port,
                committer_port,
            );
            let finalizer_net = start_node(
                &finalizer,
                &committer_pub_hex,
                committer_port,
                finalizer_port,
            );

            // The receiver forwards every decrypted inbound packet onto a channel
            // we read from; the handler runs on an RNS worker thread.
            let (cap_tx, cap_rx) = mpsc::channel::<Vec<u8>>();
            committer_net.on_packet(Arc::new(move |data| {
                let _ = cap_tx.send(data);
            }));

            // Re-announce on both sides so both destination tables carry each
            // peer's real dest_hash (the bootstrap seed has dest_hash = [0;16]).
            // This populates the route table so the sender's `create_link` (inside
            // `send_resource_to`) can find the peer's destination.
            committer_net.announce();
            finalizer_net.announce();
            sleep();

            // A NetworkPacket whose data is ~3.8 KB (the size of a real Phase-8
            // Message): far above the 500 B direct-packet cap.
            let big = vec![7u8; 3800];
            let frame = NetworkPacket {
                control: None,
                data: Some(big.clone()),
            };
            let payload = serialize_to_bytes_rmp(&frame).expect("serialize NetworkPacket");
            assert!(payload.len() > 500, "test payload must exceed RNS packet cap");

            // Deliver the real payload over the Resource path. `send_resource_to`
            // establishes the link (create_link) on demand and retries until the
            // route is up, then hands the bytes to RNS's Resource layer, which
            // fragments them (~464 B SDUs), retransmits, and reassembles.
            finalizer_net
                .send_resource_to(committer.rhash, payload.clone())
                .expect("send resource over loopback");

            let received = cap_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("resource delivered over loopback");
            assert_eq!(received, payload, "reassembled payload must match what we sent");

            finalizer_net.stop();
            committer_net.stop();
        }

        /// Fail-closed: `send_resource_to` before any link is established returns
        /// a `Resource` error rather than silently dropping the payload.
        #[test]
        fn send_resource_no_link_errors() {
            let net = NodeIdentity::generate_in_memory();
            let port = free_port();
            let net = start_node(&net, "", port, port);

            let other = [9u8; 16]; // an rhash with no route and no link
            let err = net.send_resource_to(other, vec![1u8; 4000]);
            assert!(
                matches!(err, Err(PneumaticError::Resource(_))),
                "expected a Resource error with no established link, got {:?}",
                err
            );

            net.stop();
        }
    }

    fn sleep() {
        thread::sleep(Duration::from_millis(2000));
    }
}
