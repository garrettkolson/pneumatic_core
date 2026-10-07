//! Shared fixtures for the node-registry test suite (committer convention:
//! cross-file fixtures + the recording/failing/hanging connections live
//! here, pub-ified, re-exporting the module header's test-only types).

use super::super::*;
use crate::config::BootstrapPeer;


pub fn registry_with_capacity(types: &[(NodeRegistryType, usize)]) -> NodeRegistry {
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

pub fn register_request(types: Vec<NodeRegistryType>) -> NodeRequest {
    NodeRequest {
        requester_key: vec![1],
        requester_rhash: [0u8; 16],
        request_type: NodeRequestType::Register,
        requester_types: types,
        requested_type: NodeRegistryType::Committer,
        binding_signature: vec![],
        query_target: None,
        query_nonce: None,
    }
}

/// Register `identity` with `reg` by driving the real `Register` path
/// (`handle_register`). The request is binding-signed over
/// `(identity.rhash, requested_type, types)`, so `handle_register` stores
/// the node via `with_binding` — the same binding a later directory
/// response would echo. Returns the type it was actually registered under.
pub fn register_node(
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
        query_target: None,
        query_nonce: None,
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
pub fn valid_entry(
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
pub fn envelope_signature(
    responder: &NodeIdentity,
    entries: &[NodeRegistryEntry],
    registry_type: &NodeRegistryType,
    responder_rhash: [u8; 16],
) -> Vec<u8> {
    let payload =
        directory_response_signature_payload(entries, registry_type, &responder_rhash).unwrap();
    responder.sign_message(&payload).unwrap()
}

/// Drive the real `Register` path with a multi-type binding: register
/// `identity` under `types` (all of which must be configured in `reg`),
/// signing the binding over `(rhash, requested_type, types)` so
/// `handle_register` admits the identity under each qualifying bucket.
/// Returns the identity's public key.
pub fn register_multi_bucket(
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
        query_target: None,
        query_nonce: None,
    };
    reg.handle_register(req);
    key
}

/// A `Heartbeat` binding-signed by `identity` over
/// `(identity.rhash, requested_type, types)` — the exact tuple
/// `handle_heartbeat` re-derives, so it would be accepted.
pub fn signed_heartbeat(
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
        query_target: None,
        query_nonce: None,
    }
}

// --- Phase 6.2: send_to_all observability + concurrency ---
pub use tokio::sync::mpsc;

/// A `Connection` whose `send` always fails, to exercise the failure-
/// recording arms of both fan-out methods.
pub struct FailingConnection;

#[async_trait::async_trait]
impl Connection for FailingConnection {
    async fn send(&self, _data: &Vec<u8>) -> Result<(), ConnError> {
        Err(ConnError::IO("FailingConnection.send".into()))
    }
}

/// A `Connection` whose `send` sleeps `dur` before returning `Ok`, so a
/// bounded runtime lets `send_to_all`'s elapsed-timeout arm fire on a
/// current-thread runtime.
pub struct HangingConnection {
    pub dur: Duration,
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
pub struct RecordingConnection {
    pub tx: mpsc::Sender<Vec<u8>>,
}

#[async_trait::async_trait]
impl Connection for RecordingConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), ConnError> {
        self.tx
            .try_send(data.clone())
            .map_err(|_| ConnError::IO("RecordingConnection channel full".into()))
    }
}

/// A fresh node identity. `Arc` because the peering tests hand the same
/// identity to a `Config` (which stores `Arc<NodeIdentity>`) and still need to
/// sign with and read from it.
pub fn test_identity() -> Arc<NodeIdentity> {
    Arc::new(NodeIdentity::generate_in_memory())
}

/// The `bootstrap_peers` wire form of an identity: its **RNS** public key in
/// hex, which is what `Config::bootstrap_peers` stores and what a peer's rhash
/// derives from — not the Ed25519 key.
pub fn bootstrap_peer_for(identity: &NodeIdentity, port: u16) -> BootstrapPeer {
    BootstrapPeer {
        public_key: hex::encode(identity.rns.get_public_key().expect("rns public key")),
        ip: "127.0.0.1".to_string(),
        port,
    }
}

/// A registry whose `Config` carries a *specific* identity, declared roles and
/// bootstrap peers.
///
/// [`registry_with_capacity`] mints a throwaway identity it never exposes; the
/// peering tests must control it, because a `Register` is only accepted when
/// its binding verifies against the key the sender advertises, and a bootstrap
/// peer's rhash must match the identity under test.
pub fn registry_for(
    identity: &Arc<NodeIdentity>,
    types: &[(NodeRegistryType, usize)],
    declared: &[NodeRegistryType],
    bootstrap_peers: Vec<BootstrapPeer>,
) -> NodeRegistry {
    let mut type_configs = DashMap::new();
    for (t, max) in types {
        type_configs.insert(
            t.clone(),
            NodeTypeConfig { min: 0, max: *max, min_stake: 0 },
        );
    }
    let mut config = Config::new_for_testing(
        "test_env".to_string(),
        Arc::new(DashMap::new()),
        Arc::new(type_configs),
    );
    config.identity = Arc::clone(identity);
    config.rhash = identity.rhash;
    config.public_key = identity.ed25519.public_key().expect("ed25519 public key");
    config.node_registry_types = declared.to_vec();
    config.bootstrap_peers = bootstrap_peers;
    NodeRegistry::init(Arc::new(config), None, Arc::new(|_, _| true))
}

/// How many nodes sit under `node_type`. Panics if the type has no registry,
/// which in a test means the fixture forgot to configure that type.
pub fn bucket_len(reg: &NodeRegistry, node_type: &NodeRegistryType) -> usize {
    reg.get_nodes(node_type)
        .expect("type configured in this fixture")
        .len()
}

/// A registry configured as a **directory observer** — the posture of a monitor,
/// explorer, or indexer, which holds no stake, can never register, and so would
/// otherwise be answered by peers and then discard every answer.
///
/// `declared` matters: the relaxation is refused to anything declaring a consensus
/// role, and that is the property under test in some of these cases, so it is a
/// parameter rather than a fixture default.
pub fn observer_registry_with_capacity(
    types: &[(NodeRegistryType, usize)],
    declared: &[NodeRegistryType],
) -> NodeRegistry {
    let mut type_configs = DashMap::new();
    for (t, max) in types {
        type_configs.insert(
            t.clone(),
            NodeTypeConfig { min: 0, max: *max, min_stake: 0 },
        );
    }
    let mut config = Config::new_for_testing(
        "test_env".to_string(),
        Arc::new(DashMap::new()),
        Arc::new(type_configs),
    );
    config.directory_observer = true;
    // `declared_roles` is seeded from this at init, which is exactly how the
    // binaries populate it — from the roles actually running.
    config.node_registry_types = declared.to_vec();
    NodeRegistry::init(Arc::new(config), None, Arc::new(|_, _| true))
}

/// A response from an identity that never registered with the receiver, listing
/// `listed`, built the way a real responder builds one.
pub fn unregistered_responder_response(
    responder: &NodeIdentity,
    listed: &NodeIdentity,
    node_type: &NodeRegistryType,
) -> NodeRegistryResponse {
    let entries = vec![valid_entry(listed, node_type.clone())];
    let signature = envelope_signature(responder, &entries, node_type, responder.rhash);
    NodeRegistryResponse {
        responder_key: responder.ed25519.public_key().unwrap(),
        responder_rhash: responder.rhash,
        registry_type: node_type.clone(),
        entries,
        signature,
    }
}
