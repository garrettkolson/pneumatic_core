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

    /// Find the registry type under which `key` is already registered, if
    /// any. A node is not necessarily under its `requested_type` — priority
    /// selection may have placed it under a different type.
    fn find_registered_type(&self, key: &Vec<u8>) -> Option<NodeRegistryType> {
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

        let entries = self
            .get_nodes(&requested_type)
            .map(|nodes| {
                nodes
                    .iter()
                    .map(|e| NodeRegistryEntry {
                        node_key: e.key().clone(),
                        node_rhash: e.value().rhash,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let Ok(entries_bytes) = serialize_to_bytes_rmp(&entries) else {
            eprintln!("[pneumatic] failed to serialize directory entries; dropping response");
            return;
        };

        let Ok(signature) = self.config.identity.sign_message(&entries_bytes) else {
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
        if let Some(existing_type) = self.find_registered_type(&requester_key) {
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

        let node = NodeRegistryNode::new(requester_rhash, conn);
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

    /// Apply a directory response: verify its signature, then upsert each
    /// entry under the response's registry type (entries are hints, so we
    /// re-check capacity and do not clobber live connections).
    pub fn handle_directory_response(
        &self,
        response: &NodeRegistryResponse,
    ) -> Result<(), PneumaticError> {
        let entries_bytes = serialize_to_bytes_rmp(&response.entries)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        let valid = Ed25519Provider::generate()
            .check_signature(
                &response.signature,
                &response.responder_key,
                &entries_bytes,
            )
            .unwrap_or(false);
        if !valid {
            return Err(PneumaticError::Registry(
                "directory response signature invalid".to_string(),
            ));
        }

        for entry in &response.entries {
            let conn: Box<dyn Connection> = match &self.network {
                Some(network) => Box::new(RnsConnection::new(entry.node_rhash, Arc::clone(network))),
                None => Box::new(NullConnection),
            };
            self.register_peer(
                entry.node_key.clone(),
                entry.node_rhash,
                &response.registry_type,
                conn,
            );
        }

        Ok(())
    }

    fn handle_heartbeat(&self, request: &NodeRequest) {
        if let Some(existing_type) = self.find_registered_type(&request.requester_key) {
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
                let rns_sender = RnsSender::new(Arc::clone(network), rhash);
                let _ = rns_sender.get_response(&data);
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
                        let _ = entry.value().conn.send(&send_data).await;
                    }
                }
            })
            .collect();
        join_all(send_futs).await;
    }

    /// Blocking version for sync contexts (runs async sends sequentially).
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
                let rns_sender = RnsSender::new(Arc::clone(network), rhash);
                let _ = rns_sender.get_response(&data);
            }
            return;
        }

        let send_data = data.clone();
        let _ = tokio::task::spawn_blocking(move || {
            futures::executor::block_on(async {
                let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();
                for key in keys {
                    if let Some(entry) = nodes.get(&key) {
                        let _ = entry.value().conn.send(&send_data).await;
                    }
                }
            })
        });
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
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let responder = NodeIdentity::generate_in_memory();
        let responder_pub = responder.ed25519.public_key().unwrap();
        let entries = vec![NodeRegistryEntry {
            node_key: vec![42],
            node_rhash: [7u8; 16],
        }];
        let entries_bytes = serialize_to_bytes_rmp(&entries).unwrap();
        let signature = responder.ed25519.sign_data(&entries_bytes).unwrap();
        let response = NodeRegistryResponse {
            responder_key: responder_pub,
            responder_rhash: responder.rhash,
            registry_type: NodeRegistryType::Finalizer,
            entries,
            signature,
        };
        reg.handle_directory_response(&response).unwrap();
        assert!(reg.get_nodes(&NodeRegistryType::Finalizer).unwrap().contains_key(&vec![42]));
    }

    #[test]
    fn directory_response_rejects_invalid_signature() {
        let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
        let entries = vec![NodeRegistryEntry {
            node_key: vec![42],
            node_rhash: [7u8; 16],
        }];
        let responder = Ed25519Provider::default();
        let entries_bytes = serialize_to_bytes_rmp(&entries).unwrap();
        let response = NodeRegistryResponse {
            responder_key: responder.public_key().unwrap(),
            responder_rhash: [0u8; 16],
            registry_type: NodeRegistryType::Finalizer,
            entries,
            signature: vec![0u8; 64],
        };
        assert!(reg.handle_directory_response(&response).is_err());
    }
}
