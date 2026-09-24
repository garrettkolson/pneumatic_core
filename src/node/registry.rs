use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::future::join_all;
use strum::IntoEnumIterator;

use crate::conns::{ConnError, Connection};
use crate::conns::senders::{RnsSender, Sender};
use crate::config::Config;
use crate::crypto::{AsymCryptoProvider, Ed25519Provider};
use crate::encoding::serialize_to_bytes_rmp;
use crate::errors::PneumaticError;
use crate::node::*;
use crate::rns::conn::RnsConnection;
use crate::rns::identity::NodeIdentity;
use crate::rns::wrapper::RnsNetwork;

/// Stake gate injected by the process owner. Production passes a closure
/// backed by the data service; tests pass a stub. Returns `true` when
/// `key` holds at least the minimum stake for `node_type`.
pub type StakeCheck = Arc<dyn Fn(&[u8], &NodeRegistryType) -> bool + Send + Sync>;

pub struct NodeRegistry {
    committers: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    sentinels: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    executors: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    finalizers: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    archivers: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    config: Arc<Config>,
    /// RNS transport, when enabled. `None` in tests: peers are seeded via
    /// `register_peer` with a `NullConnection`.
    network: Option<Arc<RnsNetwork>>,
    stake_check: StakeCheck,
    evictor: Mutex<Option<JoinHandle<()>>>,
    /// Set by [`stop_eviction`] / [`Drop`]; the eviction loop exits when `true`.
    shutdown: Arc<AtomicBool>,
    /// Interval between eviction passes (Phase 6.7 tightened from 10 s to 1 s).
    evict_interval: Duration,
    /// Per-(rhash, node_type) count of failed fan-out deliveries, so every lost
    /// `send_to_all` / `send_to_all_blocking` result is observable (Phase 6.2).
    /// Bounded by the number of registered nodes per type.
    delivery_failures: Arc<DashMap<([u8; 16], NodeRegistryType), u64>>,
    /// Per-send bound for the fan-out methods. Production is `SEND_TIMEOUT`;
    /// `with_send_timeout` overrides it in tests so timeout-elapsed discriminators
    /// run at ~50 ms instead of the 5 s production bound.
    send_timeout: Duration,
    /// Serializes the capacity-check + insert critical section of registration
    /// admission so two concurrent registrations cannot both observe free
    /// capacity and over-admit a type past `max_node_number` (Phase 6.3). Only
    /// the non-blocking map `len()` + `insert()` hold this; the blocking stake
    /// gate and connection setup in `handle_register` run outside it.
    admission_lock: Arc<std::sync::Mutex<()>>,
}

/// Canonical bytes for a directory response's envelope signature: the full
/// `(entries, registry_type, responder_rhash)` tuple. Shared by `handle_request`
/// (signer) and `handle_directory_response` (verifier) so the two cannot drift
/// and a signature over one (type, responder) cannot be replayed under another.
fn directory_response_signature_payload(
    entries: &[NodeRegistryEntry],
    registry_type: &NodeRegistryType,
    responder_rhash: &[u8; 16],
) -> Result<Vec<u8>, PneumaticError> {
    serialize_to_bytes_rmp(&(entries, registry_type, responder_rhash))
        .map_err(|e| PneumaticError::Encoding(e.to_string()))
}

/// Per-send bound for `NodeRegistry`'s fan-out (H7). A hung RNS route or
/// blocked socket must not wedge the caller thread indefinitely: each send is
/// capped here and degrades to `Err(ConnError::Timeout)` instead of hanging.
/// Mirrors `CONNECT_TIMEOUT_SECS` in `conns::senders`.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Eviction-loop poll interval. Tightened from 10 s to 1 s (Phase 6.7): a
/// node whose liveness has stalled is evicted within ~31 s (30 s cutoff + 1 s
/// poll) instead of ~40 s, and a `Drop`-driven shutdown returns within one
/// poll rather than up to 10 s.
const EVICTION_INTERVAL: Duration = Duration::from_secs(1);

/// Record a failed fan-out delivery (Phase 6.2): bump the per-(rhash,
/// node_type) counter and log it so every lost `send_to_all` /
/// `send_to_all_blocking` result is observable. Takes `err` by value so the
/// timeout arm can construct `ConnError::Timeout` directly. Mirrors the
/// directory-response delivery-failure log below (`{:02x?}` rhash + type).
fn record_delivery_failure(
    failures: &Arc<DashMap<([u8; 16], NodeRegistryType), u64>>,
    rhash: [u8; 16],
    node_type: &NodeRegistryType,
    err: ConnError,
) {
    failures
        .entry((rhash, node_type.clone()))
        .and_modify(|c| *c += 1)
        .or_insert(1u64);
    eprintln!(
        "[pneumatic] delivery failed to {:02x?} as {:?}: {}",
        rhash, node_type, err
    );
}

/// Run a blocking closure on a detached std thread and bound how long the
/// caller waits for it (H7). Returns `Err(ConnError::Timeout)` if the work
/// doesn't finish within `timeout`, `Err(ConnError::IO)` if the worker exits
/// before producing a result (panic/detach-drop), or the work's `Ok` result
/// otherwise. Runtime-independent — no ambient tokio runtime required — so it
/// can be used from a plain `sync` context (as opposed to `bounded_send_async`).
fn bounded_send<F, T>(timeout: Duration, work: F) -> Result<T, ConnError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<T>();
    std::thread::spawn(move || {
        let result = work();
        // Best-effort: if the caller already timed out the receiver is gone, so
        // this send fails and the result is dropped — the worker never blocks
        // past the caller's bound.
        let _ = tx.send(result);
    });

    rx.recv_timeout(timeout).map_err(|e| match e {
        std::sync::mpsc::RecvTimeoutError::Timeout => {
            ConnError::Timeout(format!("blocking send exceeded {timeout:?}"))
        }
        std::sync::mpsc::RecvTimeoutError::Disconnected => {
            ConnError::IO("send worker exited before producing a result".into())
        }
    })
}

/// Async variant of `bounded_send`: runs the blocking closure off the runtime
/// thread via `spawn_blocking` *and* bounds the wait with `time::timeout`
/// (H7), so a hung send pins neither the tokio worker nor the caller. Works on
/// both the multi-thread and the sentinel's `new_current_thread` runtimes.
async fn bounded_send_async<F, T>(timeout: Duration, work: F) -> Result<T, ConnError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::time::timeout(timeout, tokio::task::spawn_blocking(work))
        .await
        .map_err(|_| ConnError::Timeout(format!("blocking send exceeded {timeout:?}")))?
        .map_err(|_| ConnError::IO("send worker panicked before producing a result".into()))
}

impl NodeRegistry {
pub fn init(
    config: Arc<Config>,
    network: Option<Arc<RnsNetwork>>,
    stake_check: StakeCheck,
) -> Self {
    let registry = NodeRegistry {
        committers: Arc::new(DashMap::new()),
        sentinels: Arc::new(DashMap::new()),
        executors: Arc::new(DashMap::new()),
        finalizers: Arc::new(DashMap::new()),
        archivers: Arc::new(DashMap::new()),
        config,
        network: network.clone(),
        stake_check,
        evictor: Mutex::new(None),
        shutdown: Arc::new(AtomicBool::new(false)),
        evict_interval: EVICTION_INTERVAL,
        delivery_failures: Arc::new(DashMap::new()),
        send_timeout: SEND_TIMEOUT,
        admission_lock: Arc::new(std::sync::Mutex::new(())),
    };
    let mut registry = registry;
    if network.is_some() {
        registry.start_eviction();
    }
    registry
}

/// Spawn the eviction loop: remove entries not seen for 30 seconds.
fn start_eviction(&mut self) {
    let committers = Arc::clone(&self.committers);
    let sentinels = Arc::clone(&self.sentinels);
    let executors = Arc::clone(&self.executors);
    let finalizers = Arc::clone(&self.finalizers);
    let archivers = Arc::clone(&self.archivers);
    let shutdown = Arc::clone(&self.shutdown);
    let interval = self.evict_interval;
    let handle = std::thread::spawn(move || {
        loop {
            // Check before sleeping (mirrors `refresher_loop` in stake_index)
            // so the loop exits within one poll once shutdown is set, rather
            // than finishing a full sleep interval first.
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(interval);
            evict_expired(&[
                Arc::clone(&committers),
                Arc::clone(&sentinels),
                Arc::clone(&executors),
                Arc::clone(&finalizers),
                Arc::clone(&archivers),
            ]);
        }
    });
    self.evictor = Mutex::new(Some(handle));
}

/// Stop the eviction loop. Sets the shutdown flag (so the detached thread
/// exits) and joins it; [`Drop`] invokes this automatically, so a dropped
/// registry no longer leaks its five registry `Arc`s. Safe to call more than
/// once — the `.take()` makes a second call a no-op.
pub fn stop_eviction(&mut self) {
    self.shutdown.store(true, Ordering::SeqCst);
    if let Some(handle) = self.evictor.lock().unwrap().take() {
        let _ = handle.join();
    }
}

pub fn get_config(&self) -> &Arc<Config> {
    &self.config
}

pub fn get_network(&self) -> Option<Arc<RnsNetwork>> {
    self.network.as_ref().map(Arc::clone)
}

pub fn get_nodes(&self, node_type: &NodeRegistryType) -> Option<Nodes> {
    match node_type {
        NodeRegistryType::Committer => Some(Arc::clone(&self.committers)),
        NodeRegistryType::Sentinel => Some(Arc::clone(&self.sentinels)),
        NodeRegistryType::Executor => Some(Arc::clone(&self.executors)),
        NodeRegistryType::Finalizer => Some(Arc::clone(&self.finalizers)),
        NodeRegistryType::Archiver => Some(Arc::clone(&self.archivers)),
    }
}
}
#[cfg(test)]
impl NodeRegistry {
    /// Override the per-send timeout so timeout-elapsed discriminators run at a
    /// small bound instead of the 5 s production `SEND_TIMEOUT`.
    pub fn with_send_timeout(&mut self, timeout: Duration) {
        self.send_timeout = timeout;
    }

    /// Replace the injected stake gate (always-`true` in `registry_with_capacity`)
    /// so a discriminator can widen the check-then-insert window with a slow gate.
    pub fn with_stake_check(&mut self, stake: StakeCheck) {
        self.stake_check = stake;
    }

    /// Override the eviction-loop poll interval so discriminators can drive the
    /// loop at a small bound (production default is `EVICTION_INTERVAL`, 1 s).
    pub fn with_evict_interval(&mut self, interval: Duration) {
        self.evict_interval = interval;
    }

    /// Expose the eviction thread's join handle so tests can observe whether the
    /// loop has exited — the same `listening_thread` accessor pattern used on
    /// `TcpConnection`. The handle is moved out under the lock (JoinHandle is
    /// not `Clone`); since [`Drop`] and [`stop_eviction`] still drive the
    /// shutdown flag, the thread exits within one poll even when held here.
    pub fn evictor_handle(&self) -> Option<JoinHandle<()>> {
        self.evictor.lock().unwrap().take()
    }
}

/// Connection for directory entries with no live transport (test mode, or
/// peers learned via directory sync that we cannot reach directly).
pub struct NullConnection;

#[async_trait::async_trait]
impl Connection for NullConnection {
    async fn send(&self, _data: &Vec<u8>) -> Result<(), ConnError> {
        Ok(())
    }
}

/// Drop the registry cleanly. Joins the eviction thread via [`stop_eviction`]
/// (Phase 6.7): the thread had no cancellation signal and leaked for the
/// process lifetime, keeping its `Arc`s to all five registries pinned. Joining
/// — rather than detaching — lets those captured `Arc`s release, closing the
/// leak.
impl Drop for NodeRegistry {
    fn drop(&mut self) {
        self.stop_eviction();
    }
}

fn evict_expired(notes: &[Arc<DashMap<Vec<u8>, NodeRegistryNode>>]) {
    let cutoff = Instant::now() - Duration::from_secs(30);
    for nodes in notes {
        let mut expired = Vec::new();
        for entry in nodes.iter() {
            if entry.value().last_seen < cutoff {
                expired.push(entry.key().clone());
            }
        }
        for key in expired {
            nodes.remove(&key);
        }
    }
}
pub mod fanout;
pub mod heartbeat;
pub mod registration;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    pub mod helpers;
    mod fanout;
    mod heartbeat;
    mod lifecycle;
    mod registration;
}
