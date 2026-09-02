use std::sync::Arc;
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
    evictor: Option<JoinHandle<()>>,
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
            evictor: None,
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
        let handle = std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(10));
                evict_expired(&[
                    Arc::clone(&committers),
                    Arc::clone(&sentinels),
                    Arc::clone(&executors),
                    Arc::clone(&finalizers),
                    Arc::clone(&archivers),
                ]);
            }
        });
        self.evictor = Some(handle);
    }

    /// Stop the eviction loop (called before the registry is dropped).
    pub fn stop_eviction(&mut self) {
        if let Some(handle) = self.evictor.take() {
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

    pub fn node_is_already_registered(&self, key: &Vec<u8>, node_type: &NodeRegistryType) -> bool {
        match self.get_nodes(node_type) {
            Some(nodes) => nodes.contains_key(key),
            None => false,
        }
    }

    /// Return the registry type under which `key` is already registered, if
    /// any. A node is not necessarily under its `requested_type` — priority
    /// selection may have placed it under a different type. This is the
    /// committer's sender-authentication lookup: it maps an Ed25519 public key
    /// to the role that node is registered under.
    pub fn find_node_type_by_public_key(&self, key: &[u8]) -> Option<NodeRegistryType> {
        NodeRegistryType::iter().find(|t| {
            self.get_nodes(t)
                .map(|nodes| nodes.contains_key(key))
                .unwrap_or(false)
        })
    }

    /// The set of registry types `key` is registered under, in registration
    /// order (Committer, Sentinel, Executor, Finalizer, Archiver). Phase 6
    /// multi-bucket: a composite identity may register across several buckets —
    /// the `requester_types` a node declares in its binding is the full role
    /// set, and the node lands under each qualifying type. This returns that
    /// set. `find_node_type_by_public_key` is the first-match single-role view
    /// of the same lookups — both read the live map, so for any type the key
    /// holds, both agree it is present.
    pub fn find_node_types_by_public_key(&self, key: &[u8]) -> Vec<NodeRegistryType> {
        NodeRegistryType::iter()
            .filter(|t| self.get_nodes(t).map(|nodes| nodes.contains_key(key)).unwrap_or(false))
            .collect()
    }

    /// Role-set auth (Phase 6): `key` may send an action governed by
    /// `allowed_roles` iff it is registered under at least one of them — the
    /// generalized form of the old single-role `role != expected` gate. A
    /// composite identity registered as both Committer and Executor may
    /// therefore send actions for either role; an action whose sole governing
    /// role the key is not under is rejected (intersection empty ⇒ fail
    /// closed). `allowed_roles` is the `allowed_senders_for(action)` mapping
    /// for the action in question, collapsed to the set of roles permitted.
    pub fn node_may_send_action(&self, key: &[u8], allowed_roles: &[NodeRegistryType]) -> bool {
        let roles = self.find_node_types_by_public_key(key);
        !roles.is_empty() && roles.iter().any(|role| allowed_roles.contains(role))
    }

    fn type_is_maxed_out(&self, node_type: &NodeRegistryType) -> bool {
        match self.get_nodes(node_type) {
            Some(nodes) => nodes.len() >= self.config.get_max_node_number(node_type),
            None => true,
        }
    }

    /// Insert or refresh a directory entry for `key` under `node_type`.
    /// Idempotent: refreshing an existing entry updates its rhash/connection
    /// and bumps `last_seen`. Returns `false` when the type has no registry
    /// or is at capacity (and the entry is new).
    pub fn register_peer(
        &self,
        key: Vec<u8>,
        rhash: [u8; 16],
        node_type: &NodeRegistryType,
        conn: Box<dyn Connection>,
    ) -> bool {
        let Some(nodes) = self.get_nodes(node_type) else {
            return false;
        };

        if let Some(mut entry) = nodes.get_mut(&key) {
            entry.value_mut().rhash = rhash;
            entry.value_mut().conn = conn;
            entry.value_mut().last_seen = Instant::now();
            return true;
        }

        if nodes.len() >= self.config.get_max_node_number(node_type) {
            return false;
        }

        nodes.insert(key, NodeRegistryNode::new(rhash, conn));
        true
    }

    fn select_registration_node_type(&self, request: &NodeRequest) -> Option<NodeRegistryType> {
        self.select_registration_node_types(request).into_iter().next()
    }

    /// Every registry type the node qualifies for in `requester_types`, in
    /// selection priority (Finalizer > Executor > Sentinel > Committer). Phase
    /// 6 multi-bucket: the node registers under *each* qualifying type rather
    /// than the single priority one — so a composite identity lands in every
    /// bucket it qualifies for. `select_registration_node_type` is the first of
    /// these (the ack's highest-priority type).
    fn select_registration_node_types(&self, request: &NodeRequest) -> Vec<NodeRegistryType> {
        let priority = [
            NodeRegistryType::Finalizer,
            NodeRegistryType::Executor,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Committer,
        ];
        priority
            .into_iter()
            .filter(|node_type| {
                request.requester_types.contains(node_type)
                    && !self.type_is_maxed_out(node_type)
            })
            .collect()
    }

    fn can_select_this_type(&self, request: &NodeRequest, node_type: NodeRegistryType) -> bool {
        request.requester_types.contains(&node_type) && !self.type_is_maxed_out(&node_type)
    }

    /// Handle a control-plane request. The sender's rhash is claimed in
    /// `request.requester_rhash` and bound to `request.requester_key` by the
    /// binding signature — RNS is destination-encrypted and its delivery
    /// callback does not identify the sender.
    pub fn handle_control(&self, request: NodeRequest) -> Result<(), PneumaticError> {
        match &request.request_type {
            NodeRequestType::Register => {
                self.handle_register(request);
                Ok(())
            }
            NodeRequestType::RegisterAck {
                accepted,
                node_type,
                responder_key,
                reason,
            } => {
                self.handle_register_ack(
                    &request,
                    *accepted,
                    node_type.clone(),
                    responder_key,
                    reason,
                );
                Ok(())
            }
            NodeRequestType::Request => {
                self.handle_request(&request);
                Ok(())
            }
            NodeRequestType::Heartbeat => {
                self.handle_heartbeat(&request);
                Ok(())
            }
        }
    }

    /// Respond to a directory request with our entries for `requested_type`.
    fn handle_request(&self, request: &NodeRequest) {
        let requested_type = request.requested_type.clone();
        let requester_rhash = request.requester_rhash;

        // Build directory entries only from nodes we vouched for at
        // registration (a non-empty `directory_signature`). A node learned
        // via a directory response carries no binding of its own here, so we
        // cannot re-vouch for it.
        let entries: Vec<NodeRegistryEntry> = self
            .get_nodes(&requested_type)
            .map(|nodes| {
                nodes
                    .iter()
                    .filter(|e| !e.value().directory_signature.is_empty())
                    .map(|e| {
                        let v = e.value();
                        NodeRegistryEntry {
                            node_key: e.key().clone(),
                            node_rhash: v.rhash,
                            signature: v.directory_signature.clone(),
                            requested_type: v.directory_requested_type.clone(),
                            node_types: v.directory_node_types.clone(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Sign over the full (entries, registry_type, responder_rhash) tuple so
        // a valid signature over one (type, responder) cannot be replayed
        // under another.
        let Ok(payload_bytes) =
            directory_response_signature_payload(&entries, &requested_type, &self.config.rhash)
        else {
            eprintln!("[pneumatic] failed to serialize directory response; dropping response");
            return;
        };

        let Ok(signature) = self.config.identity.sign_message(&payload_bytes) else {
            eprintln!("[pneumatic] failed to sign directory response; dropping response");
            return;
        };

        let response = NodeRegistryResponse {
            responder_key: self.config.public_key.clone(),
            responder_rhash: self.config.rhash,
            registry_type: requested_type.clone(),
            entries,
            signature,
        };

        let Some(network) = &self.network else {
            return;
        };

        let Ok(packet_bytes) = serialize_to_bytes_rmp(&NetworkPacket {
            control: Some(NodeRequest {
                requester_key: self.config.public_key.clone(),
                requester_rhash: self.config.rhash,
                request_type: NodeRequestType::Request,
                requester_types: self.config.node_registry_types.clone(),
                requested_type: requested_type.clone(),
                binding_signature: vec![],
            }),
            data: Some(serialize_to_bytes_rmp(&response).unwrap_or_default()),
        }) else {
            eprintln!("[pneumatic] failed to serialize directory response; dropping response");
            return;
        };

        if packet_bytes.is_empty() {
            return;
        }

        if let Err(e) = network.send_to(requester_rhash, &packet_bytes) {
            eprintln!(
                "[pneumatic] directory response delivery to {:02x?} failed: {}",
                requester_rhash, e
            );
        }
    }

    fn handle_register(&self, request: NodeRequest) {
        let requester_key = request.requester_key.clone();
        // Claimed transport address; the binding signature below authenticates
        // that the Ed25519 key is bound to it.
        let requester_rhash = request.requester_rhash;

        if !NodeIdentity::verify_binding(
            &requester_key,
            &requester_rhash,
            &request.requested_type,
            &request.requester_types,
            &request.binding_signature,
        ) {
            self.reply_register_ack(requester_rhash, false, request.requested_type, "invalid binding signature");
            return;
        }

        // Phase 6 multi-bucket: a node declares its full role set in
        // `requester_types` (binding-signed over that very set); register it
        // under every qualifying type, not the single priority-selected one.
        // The ack still reports a single (highest-priority) type on the wire.
        let qualifying = self.select_registration_node_types(&request);

        // Idempotent re-registration: refresh liveness under every bucket the
        // key already sits under, so a live composite never ages out a role it
        // already holds.
        let already = self.find_node_types_by_public_key(&requester_key);
        for node_type in &already {
            self.refresh_last_seen(&requester_key, node_type);
        }

        // Admit the key under each qualifying type it is not already under —
        // each a fresh bucket for the same identity. The ack is accepted once
        // the node is under at least one qualifying type.
        let mut now_registered = already;
        let mut accepted = !now_registered.is_empty();
        let mut failed_stake = false;
        for node_type in &qualifying {
            if now_registered.contains(node_type) {
                accepted = true;
                continue;
            }
            let conn: Box<dyn Connection> = match &self.network {
                Some(network) => Box::new(RnsConnection::new(requester_rhash, Arc::clone(network))),
                None => Box::new(NullConnection),
            };
            if self.admit_node_under_type(&requester_key, requester_rhash, node_type.clone(), conn, &request) {
                now_registered.push(node_type.clone());
                accepted = true;
            } else {
                failed_stake = true;
            }
        }

        if accepted {
            // Ack the highest-priority type the node is actually registered
            // under now (the receiver installs the peer under this type).
            let primary = [
                NodeRegistryType::Finalizer,
                NodeRegistryType::Executor,
                NodeRegistryType::Sentinel,
                NodeRegistryType::Committer,
            ]
            .into_iter()
            .find(|t| now_registered.contains(t))
            .unwrap_or(request.requested_type);
            self.reply_register_ack(requester_rhash, true, primary, "");
        } else {
            self.reply_register_ack(
                requester_rhash,
                false,
                request.requested_type,
                if failed_stake { "insufficient stake" } else { "no registry type available" },
            );
        }
    }

    /// Admit `requester_key` under `node_type`: fresh insert (stake gate +
    /// atomic capacity check) when the key is not already present, otherwise
    /// return true (a re-registration refresh is already handled by the caller,
    /// so no second insert is needed). Returns whether the node is present
    /// under the type. The capacity check and the insert run under one lock
    /// (Phase 6.3), so the preceding stake gate and connection setup run
    /// outside it.
    fn admit_node_under_type(
        &self,
        requester_key: &Vec<u8>,
        rhash: [u8; 16],
        node_type: NodeRegistryType,
        conn: Box<dyn Connection>,
        request: &NodeRequest,
    ) -> bool {
        // Already present ⇒ a re-registration; no fresh insert needed.
        if self.node_is_already_registered(requester_key, &node_type) {
            return true;
        }

        // Stake gate against the type we will register it under. Runs OUTSIDE
        // the admission lock so it never contends the barrier.
        if !(self.stake_check)(requester_key, &node_type) {
            return false;
        }

        // Atomic admission (Phase 6.3): the capacity check and the insert run
        // under one lock, so two concurrent registrations can no longer both
        // observe free capacity and over-admit past the limit.
        let lock = Arc::clone(&self.admission_lock);
        let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        match self.get_nodes(&node_type) {
            None => false,
            Some(nodes) => {
                if nodes.len() >= self.config.get_max_node_number(&node_type) {
                    return false;
                }
                // Store the node's own rhash binding so we can vouch for it in
                // directory responses. This is the only place a node has signed
                // its rhash, so only directly-registered nodes are eligible to
                // be listed.
                let node = NodeRegistryNode::with_binding(
                    rhash,
                    conn,
                    request.binding_signature.clone(),
                    request.requested_type.clone(),
                    request.requester_types.clone(),
                );
                nodes.insert(requester_key.clone(), node);
                true
            }
        }
    }

    fn handle_register_ack(
        &self,
        request: &NodeRequest,
        accepted: bool,
        node_type: NodeRegistryType,
        responder_key: &Vec<u8>,
        reason: &String,
    ) {
        // The responder's claimed transport address.
        let responder_rhash = request.requester_rhash;

        if !accepted {
            eprintln!(
                "[pneumatic] registration as {:?} rejected by peer {:02x?}: {}",
                node_type, responder_rhash, reason
            );
            return;
        }

        // On accept the responder_key is mandatory: it is how we address the
        // peer in our own directory.
        if responder_key.is_empty() {
            eprintln!("[pneumatic] RegisterAck accepted but responder_key missing; ignoring");
            return;
        }

        // The ack is binding-signed by the responder over
        // (its rhash, node_type, its types) — the same check we apply to
        // Register requests. Verified against the ack's own `node_type` so a
        // responder cannot store us under a type it did not sign.
        if !NodeIdentity::verify_binding(
            responder_key,
            &responder_rhash,
            &node_type,
            &request.requester_types,
            &request.binding_signature,
        ) {
            eprintln!(
                "[pneumatic] RegisterAck from peer {:02x?} failed binding verification; ignoring",
                responder_rhash
            );
            return;
        }

        let conn: Box<dyn Connection> = match &self.network {
            Some(network) => Box::new(RnsConnection::new(responder_rhash, Arc::clone(network))),
            None => Box::new(NullConnection),
        };

        if !self.register_peer(responder_key.clone(), responder_rhash, &node_type, conn) {
            eprintln!(
                "[pneumatic] accepted RegisterAck for peer {:02x?} but directory is full",
                responder_rhash
            );
        }
    }

    /// Reply to a `Register` with a signed `RegisterAck`. The ack is itself a
    /// `NodeRequest`, so it carries our own binding signature over
    /// `(our rhash, node_type, our types)` — the requester verifies it before
    /// storing us. `node_type` is the type the peer was actually registered
    /// under, which may differ from its `requested_type` when priority
    /// selection chose a different type.
    fn reply_register_ack(
        &self,
        peer_rhash: [u8; 16],
        accepted: bool,
        node_type: NodeRegistryType,
        reason: &str,
    ) {
        let responder_key = if accepted {
            self.config.public_key.clone()
        } else {
            Vec::new()
        };

        let Ok(binding) = self.config.identity.sign_binding(
            &self.config.rhash,
            &node_type,
            &self.config.node_registry_types,
        ) else {
            eprintln!("[pneumatic] failed to sign RegisterAck binding; dropping ack");
            return;
        };

        let ack = NodeRequest {
            requester_key: self.config.public_key.clone(),
            requester_rhash: self.config.rhash,
            request_type: NodeRequestType::RegisterAck {
                accepted,
                node_type: node_type.clone(),
                responder_key,
                reason: reason.to_string(),
            },
            requester_types: self.config.node_registry_types.clone(),
            requested_type: node_type,
            binding_signature: binding,
        };

        let Some(network) = &self.network else {
            // Test mode: no transport to deliver on.
            return;
        };

        let Ok(packet_bytes) = serialize_to_bytes_rmp(&NetworkPacket {
            control: Some(ack),
            data: None,
        }) else {
            eprintln!("[pneumatic] failed to serialize RegisterAck; dropping ack");
            return;
        };

        if let Err(e) = network.send_to(peer_rhash, &packet_bytes) {
            eprintln!(
                "[pneumatic] RegisterAck delivery to {:02x?} failed: {}",
                peer_rhash, e
            );
        }
    }

    /// Apply a directory response. Verifies (fail-closed) that the responder is
    /// a registered node and that the envelope covers this exact
    /// `(entries, registry_type, responder_rhash)`; then that each entry is
    /// bound by its *own* listed node; then upserts the survivors. A directory
    /// response can never redirect an already-registered peer (see
    /// `register_directory_peer`).
    pub fn handle_directory_response(
        &self,
        response: &NodeRegistryResponse,
    ) -> Result<(), PneumaticError> {
        // The responder must be a registered node. Without this an attacker
        // fabricates a response signed by an arbitrary key and injects
        // arbitrary (key, rhash) mappings.
        if self.find_node_type_by_public_key(&response.responder_key).is_none() {
            return Err(PneumaticError::Registry(
                "directory response from unregistered node".to_string(),
            ));
        }

        // Envelope signature must cover the entries *under this registry type
        // and responder rhash* (see `directory_response_signature_payload`),
        // so a valid signature over one (type, responder) cannot be replayed
        // under another.
        let payload_bytes = directory_response_signature_payload(
            &response.entries,
            &response.registry_type,
            &response.responder_rhash,
        )
        .map_err(|e| PneumaticError::Encoding(e.to_string()))?;
        if !Ed25519Provider::generate()
            .check_signature(&response.signature, &response.responder_key, &payload_bytes)
            .unwrap_or(false)
        {
            return Err(PneumaticError::Registry(
                "directory response signature invalid".to_string(),
            ));
        }

        // Verify every entry is bound by its own listed node (fail closed:
        // reject the whole response on the first bad entry, before installing
        // any peer). This is what stops an attacker from attributing an
        // attacker rhash to a real key — the directory cannot forge the entry
        // node's signature.
        for entry in &response.entries {
            if !NodeIdentity::verify_binding(
                &entry.node_key,
                &entry.node_rhash,
                &entry.requested_type,
                &entry.node_types,
                &entry.signature,
            ) {
                return Err(PneumaticError::Registry(
                    "directory response entry signature invalid".to_string(),
                ));
            }
        }

        // All entries verified: install them via the refresh-only path so a
        // directory response can never overwrite an established rhash.
        for entry in &response.entries {
            let conn: Box<dyn Connection> = match &self.network {
                Some(network) => Box::new(RnsConnection::new(entry.node_rhash, Arc::clone(network))),
                None => Box::new(NullConnection),
            };
            self.register_directory_peer(
                entry.node_key.clone(),
                entry.node_rhash,
                &response.registry_type,
                conn,
            );
        }

        Ok(())
    }

    /// Register or update a peer learned via a directory response. Unlike
    /// `register_peer`, an existing key's rhash/connection is NEVER
    /// overwritten here: a directory response can only refresh liveness, never
    /// redirect an already-established peer to a new address (Phase 1.5, C7).
    /// Returns `false` when the type has no registry or is at capacity (and
    /// the entry is new).
    pub fn register_directory_peer(
        &self,
        key: Vec<u8>,
        rhash: [u8; 16],
        node_type: &NodeRegistryType,
        conn: Box<dyn Connection>,
    ) -> bool {
        let Some(nodes) = self.get_nodes(node_type) else {
            return false;
        };

        if let Some(mut existing) = nodes.get_mut(&key) {
            // Refresh liveness only — never touch rhash or conn.
            existing.value_mut().last_seen = Instant::now();
            return true;
        }

        if nodes.len() >= self.config.get_max_node_number(node_type) {
            return false;
        }

        nodes.insert(key, NodeRegistryNode::new(rhash, conn));
        true
    }

    fn handle_heartbeat(&self, request: &NodeRequest) {
        // Authenticate the binding signature over the claimed
        // `(requester_rhash, requested_type, requester_types)` before trusting
        // the key — without this any sender can refresh a registered node's
        // liveness with a forged heartbeat, making `last_seen` forgeable
        // (Phase 1.6, L1). Fail closed: reject before the registered-key
        // lookup so `refresh_last_seen` is never reached for a forged request.
        if !NodeIdentity::verify_binding(
            &request.requester_key,
            &request.requester_rhash,
            &request.requested_type,
            &request.requester_types,
            &request.binding_signature,
        ) {
            return;
        }

        if let Some(existing_type) = self.find_node_type_by_public_key(&request.requester_key) {
            self.refresh_last_seen(&request.requester_key, &existing_type);
        }
    }

    fn refresh_last_seen(&self, key: &Vec<u8>, node_type: &NodeRegistryType) {
        if let Some(nodes) = self.get_nodes(node_type) {
            if let Some(mut entry) = nodes.get_mut(key) {
                entry.value_mut().last_seen = Instant::now();
            }
        }
    }

    /// Send data to all registered nodes of a given type (async, concurrent).
    /// Each send is bounded by `self.send_timeout` so a hung route or socket
    /// can't pin a tokio worker thread (H7), and every failed delivery is
    /// recorded + logged via `record_delivery_failure` (Phase 6.2) so a lost
    /// send is observable. Both the RNS and the direct branches fan out
    /// concurrently (`join_all`).
    pub async fn send_to_all(&self, data: Vec<u8>, node_type: &NodeRegistryType) {
        let Some(nodes) = self.get_nodes(node_type) else { return };

        // If RNS transport is available, send via RNS. Fan out concurrently —
        // each peer's send is an independent `join_all` future.
        if let Some(network) = &self.network {
            // Collect rhashes to release DashMap guards
            let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();
            let mut rhashes = Vec::new();
            for key in keys {
                if let Some(entry) = nodes.get(&key) {
                    rhashes.push(entry.value().rhash);
                }
            }
            let send_futs: Vec<_> = rhashes.into_iter().map(|rhash| {
                let node_type = node_type.clone();
                let failures = Arc::clone(&self.delivery_failures);
                let network = Arc::clone(network);
                let send_data = data.clone();
                async move {
                    // Off the runtime thread (spawn_blocking) and bounded
                    // (time::timeout): a blocked RNS send degrades to
                    // Err(Timeout) instead of hanging the caller.
                    match bounded_send_async(self.send_timeout, move || {
                        let _ = RnsSender::new(network, rhash).get_response(&send_data);
                    })
                    .await
                    {
                        Ok(()) => {}
                        Err(e) => record_delivery_failure(&failures, rhash, &node_type, e),
                    }
                }
            }).collect();
            join_all(send_futs).await;
            return;
        }

        // Collect keys to release DashMap guards, then send concurrently.
        let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();

        // Use get() for each connection individually (simpler, avoids lifetime issues).
        // The `async_trait` `send` future borrows `&entry`, so the DashMap guard
        // is held across the `.await` (same as before); copy `rhash` out first.
        // Each future owns fresh clones of the shared state so it can record
        // its own result without borrowing the enclosing scope.
        let send_futs: Vec<_> = keys.into_iter()
            .map(|key| {
                let node_type = node_type.clone();
                let failures = Arc::clone(&self.delivery_failures);
                let nodes_clone = Arc::clone(&nodes);
                let send_data = data.clone();
                async move {
                    if let Some(entry) = nodes_clone.get(&key) {
                        let rhash = entry.value().rhash;
                        match tokio::time::timeout(self.send_timeout, entry.value().conn.send(&send_data)).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => record_delivery_failure(&failures, rhash, &node_type, e),
                            Err(_) => record_delivery_failure(
                                &failures, rhash, &node_type,
                                ConnError::Timeout(format!("direct send exceeded {:?}", self.send_timeout)),
                            ),
                        }
                    }
                }
            })
            .collect();
        join_all(send_futs).await;
    }

    /// Blocking version for sync contexts (runs sends sequentially). Each send
    /// is bounded by `self.send_timeout` and runs on a detached std thread, with no
    /// ambient runtime assumed (H7): a hung RNS route or socket degrades to
    /// `Err(ConnError::Timeout)` instead of hanging the caller. Every failed
    /// delivery is recorded + logged (Phase 6.2).
    pub fn send_to_all_blocking(&self, data: Vec<u8>, node_type: &NodeRegistryType) {
        let Some(nodes) = self.get_nodes(node_type) else { return };

        // If RNS transport is available, send via RNS
        if let Some(network) = &self.network {
            // Collect rhashes to release DashMap guards
            let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();
            let mut rhashes = Vec::new();
            for key in keys {
                if let Some(entry) = nodes.get(&key) {
                    rhashes.push(entry.value().rhash);
                }
            }
            let failures = Arc::clone(&self.delivery_failures);
            for rhash in rhashes {
                let network = Arc::clone(network);
                let send_data = data.clone();
                match bounded_send(self.send_timeout, move || {
                    let _ = RnsSender::new(network, rhash).get_response(&send_data);
                }) {
                    Ok(()) => {}
                    Err(e) => record_delivery_failure(&failures, rhash, node_type, e),
                }
            }
            return;
        }

        // Direct-connection branch. This path must *actually* send, not drop a
        // detached future: the `async_trait` `send` future needs a runtime to
        // drive, and a detached std thread has none. Build a self-contained
        // `current_thread` runtime here (no ambient runtime assumed — consistent
        // with 6.1) and `block_on` each send. A failed send or elapsed bound is
        // recorded + logged (Phase 6.2).
        // Direct-connection branch. This path must *actually* send, not drop a
        // detached future: the `async_trait` `send` future needs a runtime to
        // drive, and a detached std thread has none. Build a self-contained
        // `current_thread` runtime here (no ambient runtime assumed — consistent
        // with 6.1) and `block_on` each send. A failed send or elapsed bound is
        // recorded + logged (Phase 6.2).
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build local runtime for blocking direct send");

        // Collect (key, rhash) pairs up front to release DashMap guards before
        // driving each send on the local runtime.
        let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();
        let peers: Vec<(Vec<u8>, [u8; 16])> = {
            let mut v = Vec::new();
            for key in keys {
                if let Some(entry) = nodes.get(&key) {
                    v.push((key, entry.value().rhash));
                }
            }
            v
        };

        let failures = Arc::clone(&self.delivery_failures);
        for (key, rhash) in peers {
            let nodes_for_send = Arc::clone(&nodes);
            let send_data = data.clone();
            let result = runtime.block_on(async move {
                if let Some(entry) = nodes_for_send.get(&key) {
                    tokio::time::timeout(self.send_timeout, entry.value().conn.send(&send_data)).await
                } else {
                    Ok(Ok(()))
                }
            });
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => record_delivery_failure(&failures, rhash, node_type, e),
                Err(_) => record_delivery_failure(
                    &failures, rhash, node_type,
                    ConnError::Timeout(format!("direct send exceeded {:?}", self.send_timeout)),
                ),
            }
        }
    }

    /// Count of failed fan-out deliveries for a specific (rhash, node_type).
    /// Test accessor for Phase 6.2 observability.
    pub fn failure_count(&self, rhash: [u8; 16], node_type: &NodeRegistryType) -> u64 {
        self.delivery_failures
            .get(&(rhash, node_type.clone()))
            .map(|c| *c.value())
            .unwrap_or(0)
    }

    /// Total failed fan-out deliveries across every (rhash, node_type) key.
    /// Test accessor for Phase 6.2 observability.
    pub fn total_delivery_failures(&self) -> u64 {
        self.delivery_failures.iter().map(|e| *e.value()).sum()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with_capacity(types: &[(NodeRegistryType, usize)]) -> NodeRegistry {
        let mut type_configs = DashMap::new();
        for (t, max) in types {
            type_configs.insert(
                t.clone(),
                NodeTypeConfig { min: 0, max: *max, min_stake: 0 },
            );
        }
        let config = Arc::new(Config::new_for_testing(
            "test_env".to_string(),
            Arc::new(DashMap::new()),
            Arc::new(type_configs),
        ));
        NodeRegistry::init(config, None, Arc::new(|_, _| true))
    }

    fn register_request(types: Vec<NodeRegistryType>) -> NodeRequest {
        NodeRequest {
            requester_key: vec![1],
            requester_rhash: [0u8; 16],
            request_type: NodeRequestType::Register,
            requester_types: types,
            requested_type: NodeRegistryType::Committer,
            binding_signature: vec![],
        }
    }

    /// Register `identity` with `reg` by driving the real `Register` path
    /// (`handle_register`). The request is binding-signed over
    /// `(identity.rhash, requested_type, types)`, so `handle_register` stores
    /// the node via `with_binding` — the same binding a later directory
    /// response would echo. Returns the type it was actually registered under.
    fn register_node(
        reg: &NodeRegistry,
        identity: &NodeIdentity,
        requested_type: NodeRegistryType,
    ) -> NodeRegistryType {
        let types = vec![requested_type.clone()];
        let binding = identity
            .sign_binding(&identity.rhash, &requested_type, &types)
            .expect("sign binding");
        let req = NodeRequest {
            requester_key: identity.ed25519.public_key().unwrap(),
            requester_rhash: identity.rhash,
            request_type: NodeRequestType::Register,
            requester_types: types,
            requested_type,
            binding_signature: binding,
        };
        reg.handle_register(req);
        reg.find_node_type_by_public_key(&identity.ed25519.public_key().unwrap())
            .expect("node registered")
    }

    /// A per-entry binding that a node produces for its *own* rhash: the node
    /// key, its real transport address, and a signature verifying against
    /// `(node_rhash, requested_type, node_types)`. This is what a directory
    /// carries so the receiver can authenticate the (key, rhash) pair
    /// independently of the directory.
    fn valid_entry(
        identity: &NodeIdentity,
        requested_type: NodeRegistryType,
    ) -> NodeRegistryEntry {
        let node_types = vec![requested_type.clone()];
        let signature = identity
            .sign_binding(&identity.rhash, &requested_type, &node_types)
            .unwrap();
        NodeRegistryEntry {
            node_key: identity.ed25519.public_key().unwrap(),
            node_rhash: identity.rhash,
            signature,
            requested_type,
            node_types,
        }
    }

    /// The envelope signature a real responder would produce: an Ed25519 sign
    /// over `directory_response_signature_payload(entries, registry_type,
    /// responder_rhash)`, i.e. the exact bytes the receiver re-derives.
    fn envelope_signature(
        responder: &NodeIdentity,
        entries: &[NodeRegistryEntry],
        registry_type: &NodeRegistryType,
        responder_rhash: [u8; 16],
    ) -> Vec<u8> {
        let payload =
            directory_response_signature_payload(entries, registry_type, &responder_rhash).unwrap();
        responder.sign_message(&payload).unwrap()
    }

    #[test]
    fn select_prefers_finalizer_over_lower_types() {
        let reg = registry_with_capacity(&[
            (NodeRegistryType::Finalizer, 5),
            (NodeRegistryType::Executor, 5),
            (NodeRegistryType::Sentinel, 5),
            (NodeRegistryType::Committer, 5),
        ]);
        let req = register_request(vec![
            NodeRegistryType::Committer,
            NodeRegistryType::Finalizer,
        ]);
        assert_eq!(
            reg.select_registration_node_type(&req),
            Some(NodeRegistryType::Finalizer)
        );
    }

    #[test]
    fn select_falls_back_to_executor() {
        let reg = registry_with_capacity(&[
            (NodeRegistryType::Executor, 5),
            (NodeRegistryType::Committer, 5),
        ]);
        let req = register_request(vec![
            NodeRegistryType::Committer,
            NodeRegistryType::Executor,
        ]);
        assert_eq!(
            reg.select_registration_node_type(&req),
            Some(NodeRegistryType::Executor)
        );
    }

    #[test]
    fn select_skips_maxed_out_types() {
        let reg = registry_with_capacity(&[
            (NodeRegistryType::Finalizer, 1),
            (NodeRegistryType::Committer, 1),
        ]);
        // Fill the single Finalizer slot.
        assert!(reg.register_peer(
            vec![99],
            [1u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(NullConnection)
        ));
        let req = register_request(vec![
            NodeRegistryType::Committer,
            NodeRegistryType::Finalizer,
        ]);
        assert_eq!(
            reg.select_registration_node_type(&req),
            Some(NodeRegistryType::Committer)
        );
    }

    #[test]
    fn select_returns_none_when_nothing_fits() {
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 1)]);
        reg.register_peer(
            vec![99],
            [1u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(NullConnection),
        );
        let req = register_request(vec![NodeRegistryType::Finalizer]);
        assert_eq!(reg.select_registration_node_type(&req), None);
    }

    /// Drive the real `Register` path with a multi-type binding: register
    /// `identity` under `types` (all of which must be configured in `reg`),
    /// signing the binding over `(rhash, requested_type, types)` so
    /// `handle_register` admits the identity under each qualifying bucket.
    /// Returns the identity's public key.
    fn register_multi_bucket(
        reg: &NodeRegistry,
        identity: &NodeIdentity,
        requested_type: NodeRegistryType,
        types: Vec<NodeRegistryType>,
    ) -> Vec<u8> {
        let key = identity.ed25519.public_key().unwrap();
        let binding = identity
            .sign_binding(&identity.rhash, &requested_type, &types)
            .expect("sign binding");
        let req = NodeRequest {
            requester_key: key.clone(),
            requester_rhash: identity.rhash,
            request_type: NodeRequestType::Register,
            requester_types: types,
            requested_type,
            binding_signature: binding,
        };
        reg.handle_register(req);
        key
    }

    #[test]
    fn find_node_types_by_public_key_returns_full_set() {
        // Phase 6 discriminator: a composite identity registered across several
        // buckets — the set-returning lookup returns the *full* set, not just
        // the single priority-selected one. Reverting to the single-role
        // `find_node_type_by_public_key` (first-match) records only the first
        // (Committer) bucket ⇒ the Sentinel assertion fails.
        let reg = registry_with_capacity(&[
            (NodeRegistryType::Committer, 5),
            (NodeRegistryType::Sentinel, 5),
        ]);
        let id = NodeIdentity::generate_in_memory();
        register_multi_bucket(
            &reg,
            &id,
            NodeRegistryType::Committer,
            vec![NodeRegistryType::Committer, NodeRegistryType::Sentinel],
        );

        let key = id.ed25519.public_key().unwrap();
        // Registration order (Committer, Sentinel), per `NodeRegistryType::iter()`.
        assert_eq!(
            reg.find_node_types_by_public_key(&key),
            vec![NodeRegistryType::Committer, NodeRegistryType::Sentinel]
        );
        // The single-role view still agrees the node is present under Committer
        // (both read the same live map, so they cannot disagree on presence).
        assert_eq!(
            reg.find_node_type_by_public_key(&key),
            Some(NodeRegistryType::Committer)
        );
        // And the set view reports nothing for a key registered nowhere.
        assert!(reg.find_node_types_by_public_key(&vec![9, 9, 9]).is_empty());
    }

    #[test]
    fn multi_bucket_registration_same_identity() {
        // Phase 6 discriminator: a single identity registers across multiple
        // buckets in ONE Register request (its `requester_types` declares all
        // three). Reverting to single-bucket priority registration (the
        // pre-fix path installs only the highest-priority qualifying type ⇒
        // Executor) leaves the Committer + Sentinel buckets empty ⇒ both
        // assertions fail.
        let reg = registry_with_capacity(&[
            (NodeRegistryType::Committer, 5),
            (NodeRegistryType::Sentinel, 5),
            (NodeRegistryType::Executor, 5),
            (NodeRegistryType::Finalizer, 5),
        ]);
        let id = NodeIdentity::generate_in_memory();
        let key = register_multi_bucket(
            &reg,
            &id,
            NodeRegistryType::Sentinel,
            vec![
                NodeRegistryType::Committer,
                NodeRegistryType::Sentinel,
                NodeRegistryType::Executor,
            ],
        );

        // One identity, registered under every declared, qualifying bucket…
        assert!(
            reg.get_nodes(&NodeRegistryType::Committer)
                .unwrap()
                .contains_key(&key),
            "composite identity must land under Committer"
        );
        assert!(
            reg.get_nodes(&NodeRegistryType::Sentinel)
                .unwrap()
                .contains_key(&key),
            "composite identity must land under Sentinel"
        );
        assert!(
            reg.get_nodes(&NodeRegistryType::Executor)
                .unwrap()
                .contains_key(&key),
            "composite identity must land under Executor"
        );
        // …and NOT under the one type it did NOT declare.
        assert!(
            !reg.get_nodes(&NodeRegistryType::Finalizer)
                .unwrap()
                .contains_key(&key),
            "an undeclared role must not be registered"
        );
    }

    #[test]
    fn role_set_auth_rejects_foreign_action() {
        // Phase 6 discriminator: role-set auth — a key may send an action only
        // if it is registered under a role permitted to send that action. A
        // composite identity registered as BOTH Committer and Executor may send
        // either role's actions (the multi-role admission an attacker would use
        // to hide a foreign role behind a benign one); an action whose sole
        // governing role the key is not under is rejected. Reverting
        // `node_may_send_action` to a single-role `find_node_type_by_public_key`
        // check (only Committer, the first-match) reports the Executor action
        // as forbidden ⇒ the "Executor action admitted" assertion fails.
        let reg = registry_with_capacity(&[
            (NodeRegistryType::Committer, 5),
            (NodeRegistryType::Sentinel, 5),
            (NodeRegistryType::Executor, 5),
            (NodeRegistryType::Finalizer, 5),
        ]);
        let id = NodeIdentity::generate_in_memory();
        let key = id.ed25519.public_key().unwrap();

        // Composite identity: registered as Committer AND Executor.
        register_multi_bucket(
            &reg,
            &id,
            NodeRegistryType::Committer,
            vec![NodeRegistryType::Committer, NodeRegistryType::Executor],
        );

        // An Executor-governed action is admitted — even though the first-match
        // (single-role) view would only see Committer.
        assert!(
            reg.node_may_send_action(&key, &[NodeRegistryType::Executor]),
            "a composite role must be able to send its secondary role's action"
        );
        // A Committer-governed action is admitted too.
        assert!(
            reg.node_may_send_action(&key, &[NodeRegistryType::Committer]),
            "a composite role must be able to send its primary role's action"
        );
        // A Finalizer-only action is NOT permitted for this role set — rejected.
        assert!(
            !reg.node_may_send_action(&key, &[NodeRegistryType::Finalizer]),
            "an action for a role the node is not registered under must be rejected"
        );
        // An unknown key is rejected (fail closed), not admitted.
        assert!(!reg.node_may_send_action(&vec![1, 2, 3], &[NodeRegistryType::Committer]));
    }

    #[test]
    fn capacity_tracks_and_refresh_bypasses_it() {
        let reg = registry_with_capacity(&[(NodeRegistryType::Sentinel, 2)]);
        assert!(!reg.type_is_maxed_out(&NodeRegistryType::Sentinel));
        assert!(reg.register_peer(vec![1], [1u8; 16], &NodeRegistryType::Sentinel, Box::new(NullConnection)));
        assert!(reg.register_peer(vec![2], [2u8; 16], &NodeRegistryType::Sentinel, Box::new(NullConnection)));
        assert!(reg.type_is_maxed_out(&NodeRegistryType::Sentinel));
        // A new peer cannot be admitted at capacity...
        assert!(!reg.register_peer(vec![3], [3u8; 16], &NodeRegistryType::Sentinel, Box::new(NullConnection)));
        // ...but an existing peer can still refresh its entry.
        assert!(reg.register_peer(vec![1], [1u8; 16], &NodeRegistryType::Sentinel, Box::new(NullConnection)));
    }

    #[test]
    fn concurrent_admission_never_exceeds_capacity() {
        // Phase 6.3 discriminator: with many registrations racing on a small
        // capacity, the pre-fix check-then-insert TOCTOU lets every caller that
        // read `len() < cap` through the optimistic check and then insert, so
        // the type over-admits. The admission lock serializes the len-check +
        // insert so `len` can never exceed the cap.
        let cap = 20usize;
        let n = 200usize;
        let mut reg = registry_with_capacity(&[(NodeRegistryType::Sentinel, cap)]);
        // Slow the stake gate to widen the pre-fix check-then-insert window. On
        // the fixed code the gate runs OUTSIDE the admission lock, so this only
        // affects throughput, never the invariant.
        reg.with_stake_check(Arc::new(|_, _| {
            std::thread::sleep(Duration::from_millis(5));
            true
        }));
        let reg = Arc::new(reg);

        let handles = (0..n)
            .map(|_| NodeIdentity::generate_in_memory())
            .into_iter()
            .map(|id| {
                let reg = Arc::clone(&reg);
                std::thread::spawn(move || {
                    // Drive the real Register path directly (silently: a
                    // legitimate rejection under the cap must not panic a thread
                    // the way the convenience `register_node` helper does).
                    let types = vec![NodeRegistryType::Sentinel];
                    let binding = id
                        .sign_binding(&id.rhash, &NodeRegistryType::Sentinel, &types)
                        .expect("sign binding");
                    let req = NodeRequest {
                        requester_key: id.ed25519.public_key().unwrap(),
                        requester_rhash: id.rhash,
                        request_type: NodeRequestType::Register,
                        requester_types: types,
                        requested_type: NodeRegistryType::Sentinel,
                        binding_signature: binding,
                    };
                    reg.handle_register(req);
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            let _ = handle.join().expect("registration thread panicked");
        }

        let sentinels = reg.get_nodes(&NodeRegistryType::Sentinel).expect("sentinel registry");
        assert!(
            sentinels.len() <= cap,
            "capacity exceeded under concurrency: {} > {}",
            sentinels.len(),
            cap
        );
    }

    #[test]
    fn directory_response_registers_valid_entries() {
        // Happy path: a registered responder lists a peer whose per-entry
        // binding verifies. The receiver installs the peer.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let responder = NodeIdentity::generate_in_memory();
        register_node(&reg, &responder, NodeRegistryType::Finalizer);
        let listed = NodeIdentity::generate_in_memory();
        let entries = vec![valid_entry(&listed, NodeRegistryType::Finalizer)];
        let signature = envelope_signature(
            &responder,
            &entries,
            &NodeRegistryType::Finalizer,
            responder.rhash,
        );
        let response = NodeRegistryResponse {
            responder_key: responder.ed25519.public_key().unwrap(),
            responder_rhash: responder.rhash,
            registry_type: NodeRegistryType::Finalizer,
            entries,
            signature,
        };
        reg.handle_directory_response(&response).unwrap();
        assert!(reg
            .get_nodes(&NodeRegistryType::Finalizer)
            .unwrap()
            .contains_key(&listed.ed25519.public_key().unwrap()));
    }

    #[test]
    fn directory_response_rejects_invalid_signature() {
        // Responder is registered, but the envelope signature is over a
        // payload with a different responder rhash, so it does not cover the
        // (entries, type, responder_rhash) the receiver derives.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let responder = NodeIdentity::generate_in_memory();
        register_node(&reg, &responder, NodeRegistryType::Finalizer);
        let entries = vec![valid_entry(&responder, NodeRegistryType::Finalizer)];
        let bad_payload = directory_response_signature_payload(
            &entries,
            &NodeRegistryType::Finalizer,
            &[1u8; 16],
        )
        .unwrap();
        let bad_signature = responder.sign_message(&bad_payload).unwrap();
        let response = NodeRegistryResponse {
            responder_key: responder.ed25519.public_key().unwrap(),
            responder_rhash: responder.rhash,
            registry_type: NodeRegistryType::Finalizer,
            entries,
            signature: bad_signature,
        };
        assert!(reg
            .handle_directory_response(&response)
            .is_err(),
            "a signature that does not cover (entries, type, responder_rhash) must be rejected");
    }

    #[test]
    fn directory_response_rejects_unregistered_responder() {
        // The responder is not a registered node anywhere. Without the
        // responder-registration gate an attacker self-signs with an arbitrary
        // key and injects arbitrary (key, rhash) mappings.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let attacker = NodeIdentity::generate_in_memory();
        let listed = NodeIdentity::generate_in_memory();
        let entries = vec![valid_entry(&listed, NodeRegistryType::Finalizer)];
        let signature = envelope_signature(
            &attacker,
            &entries,
            &NodeRegistryType::Finalizer,
            attacker.rhash,
        );
        let response = NodeRegistryResponse {
            responder_key: attacker.ed25519.public_key().unwrap(),
            responder_rhash: attacker.rhash,
            registry_type: NodeRegistryType::Finalizer,
            entries,
            signature,
        };
        assert!(reg
            .handle_directory_response(&response)
            .is_err(),
            "a directory response from an unregistered responder must be rejected");
    }

    #[test]
    fn directory_response_rejects_real_key_attacker_rhash() {
        // The headline C7 attack: a malicious directory pairs a REAL node key
        // with an ATTACKER rhash. The directory cannot forge the real node's
        // binding, so a signature computed over the real node's own rhash does
        // NOT verify against the attacker rhash, and the response is rejected
        // — never install real_key -> attacker_rhash.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let responder = NodeIdentity::generate_in_memory();
        register_node(&reg, &responder, NodeRegistryType::Finalizer);
        let victim = NodeIdentity::generate_in_memory();
        let attacker_rhash = [9u8; 16];
        // The victim's honest binding, over its OWN rhash, forged into an entry
        // that instead claims attacker_rhash.
        let entry = NodeRegistryEntry {
            node_key: victim.ed25519.public_key().unwrap(),
            node_rhash: attacker_rhash,
            signature: victim
                .sign_binding(
                    &victim.rhash,
                    &NodeRegistryType::Finalizer,
                    &[NodeRegistryType::Finalizer],
                )
                .unwrap(),
            requested_type: NodeRegistryType::Finalizer,
            node_types: vec![NodeRegistryType::Finalizer],
        };
        let entries = vec![entry];
        let signature = envelope_signature(
            &responder,
            &entries,
            &NodeRegistryType::Finalizer,
            responder.rhash,
        );
        let response = NodeRegistryResponse {
            responder_key: responder.ed25519.public_key().unwrap(),
            responder_rhash: responder.rhash,
            registry_type: NodeRegistryType::Finalizer,
            entries,
            signature,
        };
        assert!(reg
            .handle_directory_response(&response)
            .is_err(),
            "an entry claiming attacker_rhash under a real key must be rejected");
    }

    #[test]
    fn directory_response_poisoned_cannot_change_registered_rhash() {
        // A node is already registered under real_rhash. A poisoned directory
        // response claiming attacker_rhash for that same key is rejected by the
        // per-entry check, and the already-established binding is left exactly
        // as it was — the whole response is dropped, nothing is partial.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let responder = NodeIdentity::generate_in_memory();
        register_node(&reg, &responder, NodeRegistryType::Finalizer);
        let victim = NodeIdentity::generate_in_memory();
        let real_rhash = victim.rhash;
        register_node(&reg, &victim, NodeRegistryType::Finalizer);

        let attacker_rhash = [9u8; 16];
        let entry = NodeRegistryEntry {
            node_key: victim.ed25519.public_key().unwrap(),
            node_rhash: attacker_rhash,
            signature: victim
                .sign_binding(
                    &victim.rhash,
                    &NodeRegistryType::Finalizer,
                    &[NodeRegistryType::Finalizer],
                )
                .unwrap(),
            requested_type: NodeRegistryType::Finalizer,
            node_types: vec![NodeRegistryType::Finalizer],
        };
        let entries = vec![entry];
        let signature = envelope_signature(
            &responder,
            &entries,
            &NodeRegistryType::Finalizer,
            responder.rhash,
        );
        let response = NodeRegistryResponse {
            responder_key: responder.ed25519.public_key().unwrap(),
            responder_rhash: responder.rhash,
            registry_type: NodeRegistryType::Finalizer,
            entries,
            signature,
        };

        assert!(reg.handle_directory_response(&response).is_err());
        let nodes = reg.get_nodes(&NodeRegistryType::Finalizer).unwrap();
        let stored = nodes
            .get(&victim.ed25519.public_key().unwrap())
            .expect("victim still registered");
        assert_eq!(
            stored.rhash, real_rhash,
            "a rejected poisoned response must not alter an established rhash"
        );
    }

    #[test]
    fn register_directory_peer_refresh_only() {
        // A directory response can only refresh liveness, never redirect an
        // already-registered peer to a new address.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let victim = NodeIdentity::generate_in_memory();
        register_node(&reg, &victim, NodeRegistryType::Finalizer);
        let real_rhash = victim.rhash;
        let key = victim.ed25519.public_key().unwrap();
        // Backdate liveness. The guard is dropped at the end of the block so it
        // is not held across `register_directory_peer` (which re-locks the same
        // entry).
        {
            let mut nodes = reg.get_nodes(&NodeRegistryType::Finalizer).unwrap();
            let mut stored = nodes.get_mut(&key).expect("victim registered");
            stored.value_mut().last_seen = Instant::now() - Duration::from_secs(100);
        }

        let attacker_rhash = [9u8; 16];
        assert!(reg.register_directory_peer(
            key.clone(),
            attacker_rhash,
            &NodeRegistryType::Finalizer,
            Box::new(NullConnection),
        ));
        let nodes = reg.get_nodes(&NodeRegistryType::Finalizer).unwrap();
        let stored = nodes.get(&key).expect("victim still registered");
        assert_eq!(
            stored.rhash, real_rhash,
            "register_directory_peer must never overwrite rhash"
        );
        assert!(
            stored.last_seen >= Instant::now() - Duration::from_secs(5),
            "register_directory_peer should refresh liveness to ~now"
        );
    }

    #[test]
    fn directory_response_rejects_tampered_registry_type() {
        // The envelope covers (entries, registry_type, responder_rhash).
        // Declaring a different registry_type than the one signed must fail.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let responder = NodeIdentity::generate_in_memory();
        register_node(&reg, &responder, NodeRegistryType::Finalizer);
        let entries = vec![valid_entry(&responder, NodeRegistryType::Finalizer)];
        // Sign over (entries, Finalizer, responder.rhash); declare Executor.
        let signature = envelope_signature(
            &responder,
            &entries,
            &NodeRegistryType::Finalizer,
            responder.rhash,
        );
        let response = NodeRegistryResponse {
            responder_key: responder.ed25519.public_key().unwrap(),
            responder_rhash: responder.rhash,
            registry_type: NodeRegistryType::Executor,
            entries,
            signature,
        };
        assert!(reg.handle_directory_response(&response).is_err());
    }

    /// A `Heartbeat` binding-signed by `identity` over
    /// `(identity.rhash, requested_type, types)` — the exact tuple
    /// `handle_heartbeat` re-derives, so it would be accepted.
    fn signed_heartbeat(
        identity: &NodeIdentity,
        requested_type: NodeRegistryType,
        types: Vec<NodeRegistryType>,
    ) -> NodeRequest {
        let binding = identity
            .sign_binding(&identity.rhash, &requested_type, &types)
            .expect("sign heartbeat binding");
        NodeRequest {
            requester_key: identity.ed25519.public_key().unwrap(),
            requester_rhash: identity.rhash,
            request_type: NodeRequestType::Heartbeat,
            requester_types: types,
            requested_type,
            binding_signature: binding,
        }
    }

    #[test]
    fn forged_heartbeat_does_not_refresh_last_seen() {
        // Headline L1: a forged heartbeat (right key, bogus binding) must not
        // refresh a registered node's liveness.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let victim = NodeIdentity::generate_in_memory();
        register_node(&reg, &victim, NodeRegistryType::Finalizer);
        let key = victim.ed25519.public_key().unwrap();

        // Backdate liveness so any refresh is observable.
        {
            let mut nodes = reg.get_nodes(&NodeRegistryType::Finalizer).unwrap();
            let mut stored = nodes.get_mut(&key).expect("victim registered");
            stored.value_mut().last_seen = Instant::now() - Duration::from_secs(100);
        }

        let forged = NodeRequest {
            requester_key: key.clone(),
            requester_rhash: victim.rhash,
            request_type: NodeRequestType::Heartbeat,
            requester_types: vec![NodeRegistryType::Finalizer],
            requested_type: NodeRegistryType::Finalizer,
            binding_signature: vec![0u8; 32],
        };
        reg.handle_heartbeat(&forged);

        let nodes = reg.get_nodes(&NodeRegistryType::Finalizer).unwrap();
        let stored = nodes.get(&key).expect("victim still registered");
        assert!(
            stored.last_seen < Instant::now() - Duration::from_secs(5),
            "a forged heartbeat must not refresh last_seen"
        );
    }

    #[test]
    fn authenticated_heartbeat_refreshes_last_seen() {
        // Positive control: a properly binding-signed heartbeat DOES refresh
        // liveness — proves the fix is not a silent no-op.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let victim = NodeIdentity::generate_in_memory();
        register_node(&reg, &victim, NodeRegistryType::Finalizer);
        let key = victim.ed25519.public_key().unwrap();

        {
            let mut nodes = reg.get_nodes(&NodeRegistryType::Finalizer).unwrap();
            let mut stored = nodes.get_mut(&key).expect("victim registered");
            stored.value_mut().last_seen = Instant::now() - Duration::from_secs(100);
        }

        let req = signed_heartbeat(
            &victim,
            NodeRegistryType::Finalizer,
            vec![NodeRegistryType::Finalizer],
        );
        reg.handle_heartbeat(&req);

        let nodes = reg.get_nodes(&NodeRegistryType::Finalizer).unwrap();
        let stored = nodes.get(&key).expect("victim still registered");
        assert!(
            stored.last_seen >= Instant::now() - Duration::from_secs(5),
            "a properly-signed heartbeat must refresh last_seen"
        );
    }

    #[test]
    fn heartbeat_binding_is_tuple_specific() {
        // A valid signature over one (rhash, type, types) tuple cannot be
        // replayed against a different rhash — the rhash is part of the signed
        // tuple — so relaying a captured heartbeat with a spoofed transport
        // address is rejected.
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let victim = NodeIdentity::generate_in_memory();
        register_node(&reg, &victim, NodeRegistryType::Finalizer);
        let key = victim.ed25519.public_key().unwrap();

        {
            let mut nodes = reg.get_nodes(&NodeRegistryType::Finalizer).unwrap();
            let mut stored = nodes.get_mut(&key).expect("victim registered");
            stored.value_mut().last_seen = Instant::now() - Duration::from_secs(100);
        }

        let mut req = signed_heartbeat(
            &victim,
            NodeRegistryType::Finalizer,
            vec![NodeRegistryType::Finalizer],
        );
        req.requester_rhash = [9u8; 16]; // spoofed transport address
        reg.handle_heartbeat(&req);

        let nodes = reg.get_nodes(&NodeRegistryType::Finalizer).unwrap();
        let stored = nodes.get(&key).expect("victim still registered");
        assert!(
            stored.last_seen < Instant::now() - Duration::from_secs(5),
            "a signature over a different rhash must not refresh last_seen"
        );
    }

    // --- Phase 6.1: per-send blocking I/O timeouts (H7) ---
    //
    // The RNS fan-out can't be driven through the concrete `RnsNetwork` in a
    // unit test (tests use `network: None`), so these test the bounding
    // primitives the two fan-out methods delegate to. On revert (no bound) the
    // slow-closure discriminators hang the suite instead of timing out.

    /// Positive control: a fast closure returns its result promptly.
    #[test]
    fn bounded_send_fast_returns_ok() {
        let start = Instant::now();
        let result = bounded_send(Duration::from_secs(10), || vec![1u8, 2u8, 3u8]);
        assert_eq!(result.unwrap(), vec![1u8, 2u8, 3u8]);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "fast send should return near-instantly, took {:?}",
            start.elapsed()
        );
    }

    /// Discriminator (sync): a slow closure times out instead of hanging.
    #[test]
    fn bounded_send_slow_times_out() {
        let start = Instant::now();
        let result: Result<Vec<u8>, ConnError> = bounded_send(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_millis(300));
            Vec::<u8>::new()
        });
        assert!(
            matches!(result, Err(ConnError::Timeout(_))),
            "expected a timeout, got {result:?}"
        );
        assert!(
            start.elapsed() <= Duration::from_millis(100) + Duration::from_millis(100),
            "timed out too late: took {:?}",
            start.elapsed()
        );
    }

    /// Positive control (async): a fast closure returns promptly.
    #[tokio::test]
    async fn bounded_send_async_fast_returns_ok() {
        let start = Instant::now();
        let result = bounded_send_async(Duration::from_secs(10), || vec![0u8]).await;
        assert_eq!(result.unwrap(), vec![0u8]);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "fast async send should return near-instantly, took {:?}",
            start.elapsed()
        );
    }

    /// Discriminator (async): a slow closure is cancelled at the bound rather
    /// than hanging the caller. On revert this hangs.
    #[tokio::test]
    async fn bounded_send_async_slow_times_out() {
        let start = Instant::now();
        let result: Result<Vec<u8>, ConnError> =
            bounded_send_async(Duration::from_millis(100), || {
                std::thread::sleep(Duration::from_millis(300));
                Vec::<u8>::new()
            })
            .await;
        assert!(
            matches!(result, Err(ConnError::Timeout(_))),
            "expected a timeout, got {result:?}"
        );
        assert!(
            start.elapsed() <= Duration::from_millis(100) + Duration::from_millis(100),
            "timed out too late: took {:?}",
            start.elapsed()
        );
    }

    // --- Phase 6.2: send_to_all observability + concurrency ---
    use tokio::sync::mpsc;

    /// A `Connection` whose `send` always fails, to exercise the failure-
    /// recording arms of both fan-out methods.
    struct FailingConnection;

    #[async_trait::async_trait]
    impl Connection for FailingConnection {
        async fn send(&self, _data: &Vec<u8>) -> Result<(), ConnError> {
            Err(ConnError::IO("FailingConnection.send".into()))
        }
    }

    /// A `Connection` whose `send` sleeps `dur` before returning `Ok`, so a
    /// bounded runtime lets `send_to_all`'s elapsed-timeout arm fire on a
    /// current-thread runtime.
    struct HangingConnection {
        dur: Duration,
    }

    #[async_trait::async_trait]
    impl Connection for HangingConnection {
        async fn send(&self, _data: &Vec<u8>) -> Result<(), ConnError> {
            tokio::time::sleep(self.dur).await;
            Ok(())
        }
    }

    /// A `Connection` whose `send` records the payload on an `mpsc` channel,
    /// letting a test assert that the blocking branch *actually* delivered
    /// rather than dropped the future.
    struct RecordingConnection {
        tx: mpsc::Sender<Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl Connection for RecordingConnection {
        async fn send(&self, data: &Vec<u8>) -> Result<(), ConnError> {
            self.tx
                .try_send(data.clone())
                .map_err(|_| ConnError::IO("RecordingConnection channel full".into()))
        }
    }

    /// Discriminator (async direct, `Ok(Err)` arm): three failing peers each
    /// record exactly one failure, keyed by (rhash, type). On revert (`let _ =`)
    /// nothing is recorded and the counters stay 0.
    #[tokio::test]
    async fn direct_delivery_failure_is_recorded() {
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        for (i, rhash) in [(1u8, [1u8; 16]), (2, [2u8; 16]), (3, [3u8; 16])] {
            reg.register_peer(
                vec![i],
                rhash,
                &NodeRegistryType::Finalizer,
                Box::new(FailingConnection),
            );
        }
        reg.send_to_all(vec![9u8], &NodeRegistryType::Finalizer).await;
        assert_eq!(reg.total_delivery_failures(), 3);
        for rhash in [[1u8; 16], [2u8; 16], [3u8; 16]] {
            assert_eq!(
                reg.failure_count(rhash, &NodeRegistryType::Finalizer),
                1,
                "each peer must record exactly one failure"
            );
        }
    }

    /// Discriminator (composite loopback assumption): `send_to_all` reaches a
    /// node's own connection in its own bucket — there is no self-skip. The
    /// composite node-server relies on this so cross-role messaging (e.g.
    /// Executor→Finalizer via `send_to_all(&Finalizer)`) loops back over RNS to
    /// the same process and is routed to the target role. On a revert that
    /// filters the owning key out of the fan-out, nothing arrives on the channel.
    #[tokio::test]
    async fn send_to_all_includes_self() {
        let reg = registry_with_capacity(&[(NodeRegistryType::Committer, 5)]);
        let (tx, mut rx) = mpsc::channel(16);
        // Register THIS node's own key (its real config public key) under its
        // own bucket with a spy connection — the way a composite registers
        // itself in every selected bucket.
        let own_key = reg.get_config().public_key.clone();
        reg.register_peer(
            own_key,
            [7u8; 16],
            &NodeRegistryType::Committer,
            Box::new(RecordingConnection { tx }),
        );
        reg.send_to_all(vec![5u8], &NodeRegistryType::Committer).await;
        assert_eq!(
            rx.try_recv().expect("own bucket must receive the payload"),
            vec![5u8],
            "send_to_all must not skip the node's own connection"
        );
    }

    /// Discriminator (blocking direct): the rewritten branch drives the async
    /// `send` on a local runtime instead of dropping the future, so the peer
    /// actually receives the payload. On revert (future dropped un-awaited)
    /// nothing arrives on the channel and this panics.
    #[test]
    fn blocking_direct_actually_sends_data() {
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let (tx, mut rx) = mpsc::channel(16);
        let payload = vec![42u8, 43, 44];
        reg.register_peer(
            vec![1],
            [1u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { tx }),
        );
        reg.send_to_all_blocking(payload.clone(), &NodeRegistryType::Finalizer);
        assert_eq!(
            rx.try_recv().expect("peer should have received the payload"),
            payload,
            "blocking direct branch must actually send"
        );
    }

    /// Discriminator (blocking direct, `Ok(Err)` arm): a failing peer's failure
    /// is recorded. On revert the `let _ =` swallows it and the counter stays 0.
    #[test]
    fn blocking_direct_delivery_failure_is_recorded() {
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        reg.register_peer(
            vec![1],
            [1u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(FailingConnection),
        );
        reg.send_to_all_blocking(vec![9u8], &NodeRegistryType::Finalizer);
        assert_eq!(
            reg.failure_count([1u8; 16], &NodeRegistryType::Finalizer),
            1,
            "blocking direct branch must record the failure"
        );
    }

    /// Discriminator (async direct: both `Ok(Err)` and `Err(Elapsed)` arms): a
    /// failing peer records on the `Ok(Err)` arm, a hanging peer records on the
    /// elapsed-timeout arm. On revert the elapsed peer is never recorded.
    #[tokio::test]
    async fn direct_send_timeout_is_recorded_as_failure() {
        let mut reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        reg.with_send_timeout(Duration::from_millis(50));
        reg.register_peer(
            vec![1],
            [1u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(FailingConnection),
        );
        reg.register_peer(
            vec![2],
            [2u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(HangingConnection {
                dur: Duration::from_secs(5),
            }),
        );
        reg.send_to_all(vec![9u8], &NodeRegistryType::Finalizer).await;
        assert_eq!(
            reg.failure_count([1u8; 16], &NodeRegistryType::Finalizer),
            1,
            "immediate send error (Ok(Err)) must be recorded"
        );
        assert_eq!(
            reg.failure_count([2u8; 16], &NodeRegistryType::Finalizer),
            1,
            "elapsed timeout (Err(Elapsed)) must be recorded"
        );
    }

    /// Positive control: a successful fan-out records no failures, so the
    /// counter cannot grow merely by sending.
    #[tokio::test]
    async fn successful_delivery_records_no_failure() {
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        for (i, rhash) in [(1u8, [1u8; 16]), (2, [2u8; 16])] {
            reg.register_peer(
                vec![i],
                rhash,
                &NodeRegistryType::Finalizer,
                Box::new(NullConnection),
            );
        }
        reg.send_to_all(vec![9u8], &NodeRegistryType::Finalizer).await;
        assert_eq!(
            reg.total_delivery_failures(),
            0,
            "successful deliveries must not record failures"
        );
    }

    /// Discriminator of the helper itself: failures accumulate per (rhash,
    /// type). A no-op helper reverts to all-zero.
    #[test]
    fn record_delivery_failure_counts_by_rhash_and_type() {
        let failures = Arc::new(DashMap::new());
        let ft = NodeRegistryType::Finalizer;
        record_delivery_failure(&failures, [1u8; 16], &ft, ConnError::IO("a".into()));
        record_delivery_failure(
            &failures,
            [1u8; 16],
            &ft,
            ConnError::WriteError(Some("b".into())),
        );
        record_delivery_failure(
            &failures,
            [2u8; 16],
            &ft,
            ConnError::Timeout("c".into()),
        );
        assert_eq!(
            failures
                .get(&([1u8; 16], ft.clone()))
                .map(|c| *c.value())
                .unwrap_or(0),
            2
        );
        assert_eq!(
            failures
                .get(&([2u8; 16], ft.clone()))
                .map(|c| *c.value())
                .unwrap_or(0),
            1
        );
        assert_eq!(
            failures.iter().map(|e| *e.value()).sum::<u64>(),
            3,
            "total must be the sum across keys"
        );
    }
}
