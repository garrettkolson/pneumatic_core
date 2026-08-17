//! `RnsNetwork` — pneumatic's RNS transport wrapper.
//!
//! Threading model (verified in the Stage-0 spike): the RNS driver invokes
//! `Callbacks` on its own threads, where blocking I/O is forbidden. So the
//! delivery callback does exactly one thing — a raw-size guard plus enqueue
//! — and a 4-thread worker pool does the rest (decrypt, plaintext-size
//! guard, dispatch to the application handler). Decrypting per worker is
//! fine: `Identity::decrypt` is stateless.
//!
//! Inbound channels are std `mpsc` (unbounded), one per worker — std
//! `Receiver` is not `Sync`, so a single shared queue is out. The
//! delivery callback round-robins packets across the worker queues.
//! The DoS guard is the raw-size check at the delivery→queue handoff: an
//! over-limit packet never enters a queue, so memory use stays bounded
//! by in-flight packets rather than queue depth.

use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use dashmap::DashMap;
use rns_core::packet::RawPacket;
use rns_crypto::identity::Identity;
use rns_net::{
    Callbacks, DestHash, Destination, IdentityHash, InterfaceId, NodeConfig,
    PacketHash, ProofStrategy, RnsNode,
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

/// Application handler for decrypted inbound packets. Receives raw
/// plaintext bytes; the application deserializes the `NetworkPacket`.
pub type PacketHandler = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

/// RNS transport: one `RnsNode`, a route table (rhash →
/// `AnnouncedIdentity`), and the inbound worker pool.
pub struct RnsNetwork {
    node: Arc<RnsNode>,
    my_rhash: [u8; 16],
    destinations: Arc<DashMap<[u8; 16], AnnouncedIdentity>>,
    stopped: Arc<AtomicBool>,
    handler: Arc<RwLock<Option<PacketHandler>>>,
    announce_handler: Arc<RwLock<Option<Arc<dyn Fn(AnnouncedIdentity) + Send + Sync>>>>,
    announce_rx: Mutex<Option<mpsc::Receiver<AnnouncedIdentity>>>,
    announce_worker: Mutex<Option<JoinHandle<()>>>,
    workers: Vec<JoinHandle<()>>,
}

/// Delivery callback state. Announces are inserted straight into the
/// route table (a fast DashMap insert, non-blocking); raw deliveries are
/// round-robin'd across one queue per worker.
struct NetworkCallbacks {
    txs: Vec<mpsc::Sender<Vec<u8>>>,
    next: AtomicUsize,
    destinations: Arc<DashMap<[u8; 16], AnnouncedIdentity>>,
    announce_tx: mpsc::Sender<AnnouncedIdentity>,
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
        if self.txs[idx].send(raw).is_err() {
            eprintln!("[pneumatic] rns: inbound queue closed; dropping packet");
        }
    }

    fn on_path_updated(&mut self, _dest_hash: DestHash, _hops: u8) {
        // Path changes need no pneumatic-level action in v1; sends resolve
        // the route from the current destination table at send time.
    }
}

/// `true` when a decrypted inbound plaintext is within the app-level size
/// limit (the 16 MiB frame cap, shared with the legacy framing layer).
pub fn inbound_size_ok(size: usize) -> bool {
    size <= MAX_FRAME_SIZE
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
            (0..WORKER_THREADS).map(|_| mpsc::channel::<Vec<u8>>()).unzip();
        let (announce_tx, announce_rx) = mpsc::channel::<AnnouncedIdentity>();
        let callbacks = NetworkCallbacks {
            txs,
            next: AtomicUsize::new(0),
            destinations: Arc::clone(&destinations),
            announce_tx,
        };

        let node = RnsNode::start(node_config, Box::new(callbacks))
            .map_err(|e| PneumaticError::Network(format!("rns node start: {}", e)))?;
        let node = Arc::new(node);

        let private_key = identity
            .rns
            .get_private_key()
            .ok_or_else(|| PneumaticError::CryptoError("identity has no private key".into()))?;
        let ih = IdentityHash(*identity.rns.hash());
        let dest = Destination::single_in(APP_NAME, &ASPECTS, ih)
            .set_proof_strategy(ProofStrategy::ProveAll);
        node.register_destination_with_proof(&dest, Some(private_key));
        node.announce(&dest, &identity.rns, None);

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
            stopped,
            handler,
            announce_handler: Arc::new(RwLock::new(None)),
            announce_rx: Mutex::new(Some(announce_rx)),
            announce_worker: Mutex::new(None),
            workers,
        })
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
    pub fn on_announce(&self, handler: Arc<dyn Fn(AnnouncedIdentity) + Send + Sync>) {
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
                handler(announced);
            }
        });
        *self.announce_worker.lock().unwrap() = Some(worker);
    }

    /// Clean shutdown: stop the workers, then unwrap and shut down the node
    /// (the spike verified no lingering worker refs).
    pub fn stop(self) {
        self.stopped.store(true, Ordering::SeqCst);
        for worker in self.workers {
            let _ = worker.join();
        }
        if let Some(worker) = self.announce_worker.lock().unwrap().take() {
            let _ = worker.join();
        }
        // RnsNode is not Debug, so no `.expect()` here.
        match Arc::try_unwrap(self.node) {
            Ok(node) => {
                node.shutdown();
            }
            Err(_) => {
                eprintln!("[pneumatic] rns: could not exclusively own the node at stop; other Arc holders remain");
            }
        }
    }
}

fn worker_loop(
    rx: &mpsc::Receiver<Vec<u8>>,
    stopped: &Arc<AtomicBool>,
    handler: &RwLock<Option<PacketHandler>>,
    identity: &Identity,
) {
    loop {
        if stopped.load(Ordering::SeqCst) {
            return;
        }
        let raw = match rx.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(raw) => raw,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        let Ok(packet) = RawPacket::unpack(&raw) else {
            eprintln!("[pneumatic] rns: failed to unpack inbound packet; dropping");
            continue;
        };
        let Ok(plaintext) = identity.decrypt(&packet.data) else {
            eprintln!("[pneumatic] rns: failed to decrypt inbound packet; dropping");
            continue;
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
}
