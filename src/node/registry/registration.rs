//! Registration + directory sync for `NodeRegistry`: control-message
//! dispatch, role selection under stake + capacity, node admission, the
//! register-ack round-trip, and the directory response/peer trust surface.

use super::*;

impl NodeRegistry {
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

pub(crate) fn type_is_maxed_out(&self, node_type: &NodeRegistryType) -> bool {
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

pub(crate) fn select_registration_node_type(&self, request: &NodeRequest) -> Option<NodeRegistryType> {
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

pub(crate) fn handle_register(&self, request: NodeRequest) {
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
}
