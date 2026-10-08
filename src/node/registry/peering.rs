//! Boot peering for `NodeRegistry` — the **client** half of the control plane.
//!
//! `registration.rs` already implements everything a node does when it
//! *receives* a control-plane request: verify the binding, apply the stake
//! gate, admit the peer under every qualifying type, sign the ack. What no
//! production path ever did was **send** one. `register_peer` had test-only
//! callers; `handle_register_ack` had never run outside a unit test; and the
//! only control-frame-shaped thing any binary sent was the committer's
//! directory request on announce, which was mis-framed (see
//! [`NodeRegistry::send_control`]). The consequence for a multi-process
//! cluster: every node boots cleanly, each node's role directories stay empty,
//! and nothing routes — a failure with no error message anywhere.
//!
//! Three pieces close that gap:
//!
//! 1. **Builders** (`build_*`) — pure request construction, so the exact bytes
//!    a peer is going to verify can be asserted without a socket.
//! 2. **Senders** ([`NodeRegistry::register_with_bootstrap_peers`] et al.) —
//!    deliberately thin: serialize as a `NetworkPacket` control frame and hand
//!    it to the transport.
//! 3. **A liveness loop** ([`NodeRegistry::start_peering`]) — a `Register` is
//!    the only thing that *creates* an entry, and `evict_expired`
//!    (registry.rs `evict_expired`) deletes any node not seen for 30 s. A
//!    cluster with no periodic refresh therefore dissolves its own directories
//!    about half a minute after boot, which is indistinguishable from a
//!    routing bug.
//!
//! Everything outbound describes this node with
//! [`NodeRegistry::declared_roles`] — the roles it actually serves — so a
//! committer-only binary never appears in a peer's Finalizer bucket, and an
//! epoch's role change is picked up by the next tick without restarting the
//! loop.

use super::*;
use crate::rns::identity::rhash_from_public_key;

/// Cadence of the peering loop. Must stay well inside the 30 s eviction cutoff
/// (`evict_expired`) so a healthy peer cannot age out between two refreshes:
/// three attempts per eviction window.
pub const PEERING_TICK: Duration = Duration::from_secs(10);

/// Finest sleep slice the peering loop waits in, so a shutdown is observed
/// promptly instead of after a full tick.
const PEERING_SLEEP_SLICE: Duration = Duration::from_millis(200);

impl NodeRegistry {
    /// Announce the roles this node actually serves. Call it when the role set
    /// changes — a composite right after it installs its role plugins, and
    /// again on any epoch that re-evaluates them.
    pub fn set_declared_roles(&self, roles: Vec<NodeRegistryType>) {
        *self
            .declared_roles
            .write()
            .unwrap_or_else(|p| p.into_inner()) = roles;
    }

    /// The roles this node advertises outbound. Seeded from
    /// `config.node_registry_types` at `init`.
    pub fn declared_roles(&self) -> Vec<NodeRegistryType> {
        self.declared_roles
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The type this node asks to be registered *under*, and the one it expects
    /// back in an ack: the highest-priority role it declares. Mirrors the
    /// primary-type selection the receiver makes when building the ack
    /// (registration.rs `handle_register`), so the two ends agree.
    pub fn primary_registry_type(roles: &[NodeRegistryType]) -> Option<NodeRegistryType> {
        [
            NodeRegistryType::Finalizer,
            NodeRegistryType::Executor,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Committer,
        ]
        .into_iter()
        .find(|t| roles.contains(t))
    }

    /// The role buckets a node needs directory entries for — every role the
    /// pipeline routes *to*. `Archiver` is excluded: no role plugin builds one
    /// and no pipeline path sends to it.
    pub fn routed_role_types() -> [NodeRegistryType; 4] {
        [
            NodeRegistryType::Sentinel,
            NodeRegistryType::Executor,
            NodeRegistryType::Finalizer,
            NodeRegistryType::Committer,
        ]
    }

    /// Build (but do not send) the `Register` announcing this node under its
    /// declared roles. The binding covers `(our rhash, requested_type,
    /// declared_roles)` — the exact triple `handle_register` re-verifies — and
    /// it signs the role set itself, so a peer cannot move us into a bucket we
    /// did not claim.
    ///
    /// Fails closed on an empty role set: a `Register` claiming no roles can
    /// only be rejected, and sending one would relabel a caller bug (a
    /// composite that selected no roles) as a peering failure.
    pub fn build_register_request(&self) -> Result<NodeRequest, PneumaticError> {
        let declared = self.declared_roles();
        let requested_type = Self::primary_registry_type(&declared).ok_or_else(|| {
            PneumaticError::Registry(
                "peering: cannot register with an empty declared role set".to_string(),
            )
        })?;
        self.build_request(NodeRequestType::Register, requested_type, &declared)
    }

    /// Build a directory request for the `for_type` bucket. The declared roles
    /// describe *us* to the responder; the bucket asked about is `for_type`.
    /// Ask one specific peer (`responder_rhash`) for `for_type`.
    ///
    /// Unlike `Register`/`Heartbeat`, this signs with [`NodeIdentity::sign_query`]
    /// rather than `sign_binding`, and carries the responder's rhash plus a
    /// one-shot nonce. Two reasons, both about replay:
    ///
    /// - the classic binding covers no responder, so one captured query could be
    ///   sent to *every* peer we know;
    /// - it covers no nonce or timestamp, so it could be sent *forever*.
    ///
    /// A query is repeatable and expensive — a 40-entry directory is ~156 KB of
    /// hybrid signatures plus an ML-DSA sign to produce — so the difference
    /// matters. Peers that already registered with us may still accept a legacy
    /// query with these fields `None` (rolling upgrade); an unregistered observer
    /// is only ever answered with them present.
    pub fn build_directory_request(
        &self,
        for_type: &NodeRegistryType,
        responder_rhash: [u8; 16],
    ) -> Result<NodeRequest, PneumaticError> {
        let declared = self.declared_roles();
        let mut nonce = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
        let binding_signature =
            self.config
                .identity
                .sign_query(&responder_rhash, &nonce, for_type, &declared)?;
        Ok(NodeRequest {
            requester_key: self.config.public_key.clone(),
            requester_rhash: self.config.rhash,
            request_type: NodeRequestType::Request,
            requester_types: declared,
            requested_type: for_type.clone(),
            binding_signature,
            query_target: Some(responder_rhash),
            query_nonce: Some(nonce),
        })
    }

    /// Build a `Heartbeat`: an authenticated `last_seen` refresh. This is the
    /// only refresh path for a peer learned from a *directory response* — it
    /// was never registered here, so nothing but a heartbeat (or a
    /// re-`Register` we did not send) keeps it from aging out.
    pub fn build_heartbeat_request(&self) -> Result<NodeRequest, PneumaticError> {
        let declared = self.declared_roles();
        let requested_type = Self::primary_registry_type(&declared).ok_or_else(|| {
            PneumaticError::Registry(
                "peering: cannot heartbeat with an empty declared role set".to_string(),
            )
        })?;
        self.build_request(NodeRequestType::Heartbeat, requested_type, &declared)
    }

    /// Shared construction: sign the binding over exactly the triple the peer
    /// will verify, then fill the request. A signing failure propagates rather
    /// than degrading into an empty `binding_signature` that every peer rejects
    /// — that would turn a local identity fault into a cluster-wide mystery
    /// (same reasoning as the committer's announce handler, main.rs:216).
    fn build_request(
        &self,
        request_type: NodeRequestType,
        requested_type: NodeRegistryType,
        declared: &[NodeRegistryType],
    ) -> Result<NodeRequest, PneumaticError> {
        let binding_signature =
            self.config
                .identity
                .sign_binding(&self.config.rhash, &requested_type, declared)?;
        Ok(NodeRequest {
            requester_key: self.config.public_key.clone(),
            requester_rhash: self.config.rhash,
            request_type,
            requester_types: declared.to_vec(),
            requested_type,
            binding_signature,
            // `Register` and `Heartbeat` are not queries. They keep the classic
            // binding — one registration happens once per peer, so the absence of
            // a nonce costs little; a directory query happens forever, which is
            // why only that path binds a responder and a nonce.
            query_target: None,
            query_nonce: None,
        })
    }

    /// Deliver one control-plane request to one peer.
    ///
    /// **Framing.** A control request must ride inside a [`NetworkPacket`] under
    /// `control`, which is how both binaries dispatch. Sending the bare
    /// `NodeRequest` — what the committer's announce handler used to do —
    /// decodes *successfully* as a `NetworkPacket` with both fields `None` (rmp
    /// named maps ignore unknown keys, and serde reads an absent `Option` field
    /// as `None`), so the packet is dropped with no error and no log line.
    /// Pinned by `bare_node_request_bytes_are_a_black_hole`.
    ///
    /// **Path.** Size routing and the not-live-route decision live in
    /// [`RnsNetwork::send_control_frame`], next to the transport's other
    /// senders: a control frame is ~5.9 KB (the hybrid binding signature), so it
    /// can only ride the Resource path, which needs a live route — and a route
    /// seeded from `bootstrap_peers` is not one until the peer's announce
    /// arrives. "Not yet" is returned rather than waited on; this is a retry
    /// loop.
    pub fn send_control(
        &self,
        peer_rhash: [u8; 16],
        request: NodeRequest,
    ) -> Result<(), PneumaticError> {
        let Some(network) = &self.network else {
            return Err(PneumaticError::Registry(
                "peering: no transport configured (registry is in test mode)".to_string(),
            ));
        };
        let frame = NetworkPacket {
            control: Some(request),
            data: None,
        };
        network.send_control_frame(peer_rhash, &frame)
    }

    /// Transport addresses of the configured bootstrap peers, as
    /// `(rhash, human-readable description)`.
    ///
    /// `Config::bootstrap_peers` stores the hex RNS public key — the same field
    /// `RnsNetwork::start` pre-seeds routes from — so the rhash is derived
    /// rather than configured, and cannot disagree with the transport's view.
    /// A key that will not parse is skipped with a log line: one malformed
    /// entry in a 40-entry list must not cost the node all its other peering.
    pub fn bootstrap_peer_rhashes(&self) -> Vec<([u8; 16], String)> {
        let mut peers = Vec::new();
        for peer in &self.config.bootstrap_peers {
            match rhash_from_hex_public_key(&peer.public_key) {
                Some(rhash) => peers.push((rhash, format!("{}:{}", peer.ip, peer.port))),
                None => eprintln!(
                    "[pneumatic] skipping bootstrap peer {} ({:?}): public_key is not 64 bytes of hex",
                    peer.ip, peer.public_key
                ),
            }
        }
        peers
    }

    /// Register with one specific peer. Used by the announce handlers, where a
    /// route has just become live — exactly when [`NodeRegistry::send_control`]
    /// can deliver a frame of this size.
    pub fn register_with_peer(&self, peer_rhash: [u8; 16]) -> Result<(), PneumaticError> {
        self.send_control(peer_rhash, self.build_register_request()?)
    }

    /// Ask one specific peer for every routable role bucket; returns how many
    /// requests landed.
    pub fn request_directories_from_peer(&self, peer_rhash: [u8; 16]) -> usize {
        let mut sent = 0;
        for for_type in Self::routed_role_types() {
            let request = match self.build_directory_request(&for_type, peer_rhash) {
                Ok(request) => request,
                Err(e) => {
                    eprintln!(
                        "[pneumatic] peering: cannot build directory request for {for_type:?}: {e}"
                    );
                    return sent;
                }
            };
            match self.send_control(peer_rhash, request) {
                Ok(()) => sent += 1,
                Err(e) => eprintln!(
                    "[pneumatic] peering: directory request ({for_type:?}) to {peer_rhash:02x?} failed: {e}"
                ),
            }
        }
        sent
    }

    /// Send a `Register` to every bootstrap peer; returns how many were sent.
    ///
    /// One signature serves every peer: the binding covers our own
    /// `(rhash, requested_type, roles)` and nothing about the receiver, so the
    /// same signed bytes are valid for all of them. Re-signing per peer would
    /// buy nothing — the request carries our identity either way.
    pub fn register_with_bootstrap_peers(&self) -> usize {
        let peers = self.bootstrap_peer_rhashes();
        if peers.is_empty() {
            return 0;
        }
        let mut sent = 0;
        for (rhash, desc) in peers {
            if let Err(e) = self.register_with_peer(rhash) {
                eprintln!("[pneumatic] peering: Register to {desc} failed: {e}");
            } else {
                sent += 1;
            }
        }
        sent
    }

    /// Ask every bootstrap peer for each routable role bucket; returns how many
    /// requests were sent.
    ///
    /// Registration is pairwise and only teaches a peer about *us*; a directory
    /// request is how we learn about everyone else it has admitted. Without it
    /// a node knows only its own bootstrap list, and cluster size is capped by
    /// the density of that list.
    pub fn request_directories_from_bootstrap_peers(&self) -> usize {
        self.bootstrap_peer_rhashes()
            .into_iter()
            .map(|(rhash, _desc)| self.request_directories_from_peer(rhash))
            .sum()
    }

    /// Bootstrap peers whose rhash we do not hold in ANY of our own buckets
    /// yet — i.e. peers we have not learned. Pure decision: no transport, so
    /// the selection logic is testable without a network.
    pub fn bootstrap_peers_needing_directory(&self) -> Vec<([u8; 16], String)> {
        let held: std::collections::HashSet<[u8; 16]> =
            self.known_peers().into_iter().map(|(rhash, _key)| rhash).collect();
        self.bootstrap_peer_rhashes()
            .into_iter()
            .filter(|(rhash, _desc)| !held.contains(rhash))
            .collect()
    }

    /// Re-request directories from exactly those bootstrap peers we still have
    /// not learned, until every one of them lives in our directories.
    ///
    /// Why this exists next to the first-round [`Self::
    /// request_directories_from_bootstrap_peers`]: a directory request can only
    /// be delivered once the responder's announce has made our route to it
    /// live, and a booting node peering before that moment has its one-shot
    /// requests die on "no live route". Register retries per tick, but the
    /// Register *ack* only carries the responder's own entry — so the late
    /// node ends up with live routes, no directories, and nothing left to
    /// retry. Observed 10/08/2026 in the multi-host rehearsal: a restarted
    /// container validated all three peers' announces, kept routes alive for
    /// minutes, and held `pneumatic_node_peers 0` forever. The retry stops on
    /// its own — once every bootstrap peer is held, this sends nothing.
    pub fn catch_up_directories_from_bootstrap_peers(&self) -> usize {
        self.bootstrap_peers_needing_directory()
            .into_iter()
            .map(|(rhash, _desc)| self.request_directories_from_peer(rhash))
            .sum()
    }

    /// Every peer we currently hold, deduplicated by public key, as
    /// `(rhash, key)`.
    ///
    /// Dedup matters: a composite registered under four roles sits in four
    /// maps, and without it it would be sent four heartbeats every tick.
    pub fn known_peers(&self) -> Vec<([u8; 16], Vec<u8>)> {
        let mut seen = std::collections::HashSet::new();
        let mut peers = Vec::new();
        for node_type in Self::routed_role_types() {
            let Some(nodes) = self.get_nodes(&node_type) else {
                continue;
            };
            for entry in nodes.iter() {
                if seen.insert(entry.key().clone()) {
                    peers.push((entry.value().rhash, entry.key().clone()));
                }
            }
        }
        peers
    }

    /// Send a `Heartbeat` to every peer in our directories; returns how many
    /// were sent.
    pub fn heartbeat_known_peers(&self) -> usize {
        let request = match self.build_heartbeat_request() {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[pneumatic] peering: cannot build Heartbeat: {e}");
                return 0;
            }
        };
        let mut sent = 0;
        for (rhash, _key) in self.known_peers() {
            // A peer that has moved on is normal here, not a reason to stop:
            // keep refreshing the rest of the directory.
            if self.send_control(rhash, request.clone()).is_ok() {
                sent += 1;
            }
        }
        sent
    }

    /// Start the peering loop: register with bootstrap peers, ask them for
    /// directories, then repeat on [`PEERING_TICK`] — re-registering (which is
    /// idempotent, refreshes our liveness, and re-adds us after a peer
    /// restart) and heartbeating the peers learned from directories.
    ///
    /// Returns whether the loop started: `false` with no transport, no
    /// declared roles, or a loop already running. A node that serves no roles
    /// has nothing to advertise, and registering it would only pollute its
    /// peers' buckets.
    ///
    /// The thread holds a `Weak` reference, so dropping the registry ends the
    /// loop; [`NodeRegistry::stop_peering`] (called from `Drop`) joins it.
    pub fn start_peering(self: &Arc<Self>) -> bool {
        self.start_peering_with_tick(PEERING_TICK)
    }

    /// [`NodeRegistry::start_peering`] with an explicit tick, for tests.
    pub fn start_peering_with_tick(self: &Arc<Self>, tick: Duration) -> bool {
        if self.network.is_none() {
            return false;
        }
        if self.declared_roles().is_empty() {
            eprintln!("[pneumatic] peering: no roles declared; not starting the peering loop");
            return false;
        }
        // Never two loops for one registry: the second would double the
        // control traffic and, in a test, mask whether the first is alive.
        if self.peering.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
            return false;
        }

        let weak = Arc::downgrade(self);
        let shutdown = Arc::clone(&self.shutdown);
        let handle = std::thread::spawn(move || {
            let mut first_round = true;
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                // `upgrade` returns `None` once the last strong `Arc` is gone
                // (during `Drop` the strong count is already zero), which is how
                // the loop stops when its owner goes away.
                let Some(registry) = weak.upgrade() else {
                    return;
                };

                // Roles are read per round, so an epoch that changes which
                // plugins this node runs is advertised without restarting.
                let registered = registry.register_with_bootstrap_peers();
                if first_round {
                    let directories = registry.request_directories_from_bootstrap_peers();
                    eprintln!(
                        "[pneumatic] peering: registered with {registered} bootstrap peer(s), requested {directories} directories"
                    );
                    first_round = false;
                } else {
                    registry.heartbeat_known_peers();
                    // Re-request directories from bootstrap peers we still have
                    // not learned. This is the retry the first-round-only fetch
                    // could not be: on a restart those early requests died on
                    // "no live route", and once the peer's announce restores the
                    // route there is nothing else that pulls the directory back
                    // in. It goes quiet by itself once every bootstrap peer is
                    // held (see `catch_up_directories_from_bootstrap_peers`).
                    registry.catch_up_directories_from_bootstrap_peers();
                }
                drop(registry);

                let mut slept = Duration::ZERO;
                while slept < tick {
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(PEERING_SLEEP_SLICE);
                    slept += PEERING_SLEEP_SLICE;
                }
            }
        });
        *self.peering.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
        true
    }

    /// Stop and join the peering loop. Safe to call more than once.
    ///
    /// Only joins: the shutdown flag is the eviction loop's
    /// ([`NodeRegistry::stop_eviction`] sets it), so calling this before that
    /// leaves the loop running.
    pub fn stop_peering(&mut self) {
        if let Some(handle) = self
            .peering
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }
}

/// Derive a peer's transport address from the hex RNS public key stored in
/// `Config::bootstrap_peers`. `None` for anything that is not exactly 64 bytes
/// of hex; callers log and skip.
fn rhash_from_hex_public_key(hex_key: &str) -> Option<[u8; 16]> {
    let bytes = hex::decode(hex_key.trim()).ok()?;
    let key: [u8; 64] = bytes.try_into().ok()?;
    Some(rhash_from_public_key(&key))
}
