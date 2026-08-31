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
        if self.can_select_this_type(request, NodeRegistryType::Finalizer) {
            return Some(NodeRegistryType::Finalizer);
        }

        if self.can_select_this_type(request, NodeRegistryType::Executor) {
            return Some(NodeRegistryType::Executor);
        }

        if self.can_select_this_type(request, NodeRegistryType::Sentinel) {
            return Some(NodeRegistryType::Sentinel);
        }

        if self.can_select_this_type(request, NodeRegistryType::Committer) {
            return Some(NodeRegistryType::Committer);
        }

        None
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

        // Idempotent re-registration: refresh liveness under the type we
        // already hold it, re-ack. Checked across all types — the node may
        // sit under a priority-selected type, not its `requested_type`.
        if let Some(existing_type) = self.find_node_type_by_public_key(&requester_key) {
            self.refresh_last_seen(&requester_key, &existing_type);
            self.reply_register_ack(requester_rhash, true, existing_type, "");
            return;
        }

        let Some(node_type) = self.select_registration_node_type(&request) else {
            self.reply_register_ack(
                requester_rhash,
                false,
                request.requested_type,
                "no registry type available",
            );
            return;
        };

        // Stake gate against the type we will actually register it under.
        if !(self.stake_check)(&requester_key, &node_type) {
            self.reply_register_ack(requester_rhash, false, node_type, "insufficient stake");
            return;
        }

        let conn: Box<dyn Connection> = match &self.network {
            Some(network) => Box::new(RnsConnection::new(requester_rhash, Arc::clone(network))),
            None => Box::new(NullConnection),
        };

        // Store the node's own rhash binding so we can vouch for it in
        // directory responses. This is the only place a node has signed its
        // rhash, so only directly-registered nodes are eligible to be listed.
        let node = NodeRegistryNode::with_binding(
            requester_rhash,
            conn,
            request.binding_signature.clone(),
            request.requested_type.clone(),
            request.requester_types.clone(),
        );
        match self.get_nodes(&node_type) {
            None => {
                self.reply_register_ack(requester_rhash, false, node_type, "no registry for type");
            }
            Some(nodes) => {
                nodes.insert(requester_key, node);
                self.reply_register_ack(requester_rhash, true, node_type, "");
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
    /// Each send is bounded by `SEND_TIMEOUT` so a hung route or socket can't
    /// pin a tokio worker thread (H7).
    pub async fn send_to_all(&self, data: Vec<u8>, node_type: &NodeRegistryType) {
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
            for rhash in rhashes {
                let network = Arc::clone(network);
                let send_data = data.clone();
                // Off the runtime thread (spawn_blocking) and bounded
                // (time::timeout): a blocked RNS send degrades to Err(Timeout)
                // instead of hanging the caller.
                let _ = bounded_send_async(SEND_TIMEOUT, move || {
                    let _ = RnsSender::new(network, rhash).get_response(&send_data);
                })
                .await;
            }
            return;
        }

        // Collect keys to release DashMap guards, then send
        let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();

        // Use get() for each connection individually (simpler, avoids lifetime issues)
        let send_futs: Vec<_> = keys.into_iter()
            .map(|key| {
                let nodes_clone = Arc::clone(&nodes);
                let send_data = data.clone();
                async move {
                    if let Some(entry) = nodes_clone.get(&key) {
                        let send = entry.value().conn.send(&send_data);
                        let _ = tokio::time::timeout(SEND_TIMEOUT, send).await;
                    }
                }
            })
            .collect();
        join_all(send_futs).await;
    }

    /// Blocking version for sync contexts (runs sends sequentially). Each send
    /// is bounded by `SEND_TIMEOUT` and runs on a detached std thread, with no
    /// ambient runtime assumed (H7): a hung RNS route or socket degrades to
    /// `Err(ConnError::Timeout)` instead of hanging the caller.
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
            for rhash in rhashes {
                let network = Arc::clone(network);
                let send_data = data.clone();
                let _ = bounded_send(SEND_TIMEOUT, move || {
                    let _ = RnsSender::new(network, rhash).get_response(&send_data);
                });
            }
            return;
        }

        // Bound each blocking send independently on a detached std thread.
        // (Replaces the previous spawn_blocking + block_on "double-box", which
        // pinned the ambient runtime and was unbounded.)
        let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();
        for key in keys {
            let nodes_clone = Arc::clone(&nodes);
            let send_data = data.clone();
            let _ = bounded_send(SEND_TIMEOUT, move || {
                if let Some(entry) = nodes_clone.get(&key) {
                    let _ = entry.value().conn.send(&send_data);
                }
            });
        }
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
}
