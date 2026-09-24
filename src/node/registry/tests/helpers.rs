//! Shared fixtures for the node-registry test suite (committer convention:
//! cross-file fixtures + the recording/failing/hanging connections live
//! here, pub-ified, re-exporting the module header's test-only types).

use super::super::*;


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
