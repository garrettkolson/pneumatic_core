//! Registering tests for the Sentinel, moved from `crate::sentinel::tests`.

use super::helpers::*;
use super::super::*;


// --- handle_register_request tests ---

#[test]
fn handle_register_request_with_sufficient_stake_succeeds() {
    let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let key = identity.ed25519.public_key().unwrap();
    let mut data_provider = StubDataProvider::new();
    data_provider = data_provider.with_user(
        key.clone(),
        "test".to_string(),
        User { public_key: key.clone(), fuel_balance: 1000, stake: 100, nonce: 0 },
    );
    let (sentinel, _registry) = make_sentinel_fixture_with_data_provider(data_provider);

    let req = NodeRegistryRequest::new(
        key.clone(),
        identity.rhash,
        identity
            .sign_binding(&identity.rhash, &NodeRegistryType::Sentinel, &[NodeRegistryType::Sentinel])
            .unwrap(),
        vec![NodeRegistryType::Sentinel],
        NodeRegistryType::Sentinel,
    );
    let msg = Message {
        chain_id: "test".into(),
        action: "Register".into(),
        body: serialize_to_bytes_rmp(&req).unwrap(),
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_ok());

    // Verify the node was added to the sentinel registry.
    let nodes = sentinel.node_registry.get_nodes(&NodeRegistryType::Sentinel).unwrap();
    assert!(nodes.contains_key(&key));
}


#[test]
fn handle_register_request_already_registered_returns_error() {
    let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let key = identity.ed25519.public_key().unwrap();
    let mut data_provider = StubDataProvider::new();
    data_provider = data_provider.with_user(
        key.clone(),
        "test".to_string(),
        User { public_key: key.clone(), fuel_balance: 1000, stake: 100, nonce: 0 },
    );
    let (sentinel, _registry) = make_sentinel_fixture_with_data_provider(data_provider);

    // Pre-register the node.
    let nodes = sentinel.node_registry.get_nodes(&NodeRegistryType::Sentinel).unwrap();
    nodes.insert(key.clone(),
        pneumatic_core::node::NodeRegistryNode::new(
            [0u8; 16],
            Box::new(pneumatic_core::node::registry::NullConnection),
        ));

    let req = NodeRegistryRequest::new(
        key.clone(),
        identity.rhash,
        identity
            .sign_binding(&identity.rhash, &NodeRegistryType::Sentinel, &[NodeRegistryType::Sentinel])
            .unwrap(),
        vec![NodeRegistryType::Sentinel],
        NodeRegistryType::Sentinel,
    );
    let msg = Message {
        chain_id: "test".into(),
        action: "Register".into(),
        body: serialize_to_bytes_rmp(&req).unwrap(),
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_err());
    match result.unwrap_err() {
        SentinelError::Registry(msg) => assert!(msg.contains("already registered")),
        _ => panic!("expected Registry error"),
    }
}


#[test]
fn handle_register_request_insufficient_stake_returns_error() {
    let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let key = identity.ed25519.public_key().unwrap();
    let mut data_provider = StubDataProvider::new();
    // Stake of 1 < default min_stake of 10
    data_provider = data_provider.with_user(
        key.clone(),
        "test".to_string(),
        User { public_key: key.clone(), fuel_balance: 0, stake: 1, nonce: 0 },
    );
    let (sentinel, _registry) = make_sentinel_fixture_with_data_provider(data_provider);

    let req = NodeRegistryRequest::new(
        key.clone(),
        identity.rhash,
        identity
            .sign_binding(&identity.rhash, &NodeRegistryType::Sentinel, &[NodeRegistryType::Sentinel])
            .unwrap(),
        vec![NodeRegistryType::Sentinel],
        NodeRegistryType::Sentinel,
    );
    let msg = Message {
        chain_id: "test".into(),
        action: "Register".into(),
        body: serialize_to_bytes_rmp(&req).unwrap(),
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_err());
    match result.unwrap_err() {
        SentinelError::Registry(msg) => assert!(msg.contains("Insufficient stake")),
        _ => panic!("expected Registry error"),
    }
}


/// A well-staked user (stake 1000) passes both the global floor (10) and the
/// Sentinel per-type floor (10) in the default fixture — registration
/// succeeds. Guards the happy path against the AND-semantics refactor.
#[test]
fn handle_register_request_sufficient_stake_passes() {
    let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let key = identity.ed25519.public_key().unwrap();
    let mut data_provider = StubDataProvider::new();
    data_provider = data_provider.with_user(
        key.clone(),
        "test".to_string(),
        User { public_key: key.clone(), fuel_balance: 1000, stake: 1000, nonce: 0 },
    );
    let (sentinel, _registry) = make_sentinel_fixture_with_data_provider(data_provider);

    let req = NodeRegistryRequest::new(
        key.clone(),
        identity.rhash,
        identity
            .sign_binding(&identity.rhash, &NodeRegistryType::Sentinel, &[NodeRegistryType::Sentinel])
            .unwrap(),
        vec![NodeRegistryType::Sentinel],
        NodeRegistryType::Sentinel,
    );
    let msg = Message {
        chain_id: "test".into(),
        action: "Register".into(),
        body: serialize_to_bytes_rmp(&req).unwrap(),
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    // Register succeeds (no "already registered" error ⇒ the node was added).
    assert!(sentinel.on_data_received(raw.clone()).is_ok());
    // A second register for the same type is rejected as already-registered.
    assert!(sentinel.on_data_received(raw.clone()).is_err());
}


/// Discriminator for the global-floor enforcement. The Sentinel per-type floor
/// is lowered to 1 while the global floor stays at 10; a user with stake 5
/// therefore passes the per-type floor alone (5 ≥ 1) but fails the global
/// floor (5 < 10). The OLD `user.stake >= min_stake` logic accepts it; the NEW
/// `meets_minimum_stake(5, 10, 1)` AND rejects it. Asserts rejection ⇒ proves
/// the global floor is now enforced.
#[test]
fn handle_register_request_below_global_floor_rejected() {
    let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let key = identity.ed25519.public_key().unwrap();
    let mut data_provider = StubDataProvider::new();
    data_provider = data_provider.with_user(
        key.clone(),
        "test".to_string(),
        User { public_key: key.clone(), fuel_balance: 1000, stake: 5, nonce: 0 },
    );
    let (sentinel, _registry) = make_sentinel_fixture_sentinel_floor(1, data_provider);

    let req = NodeRegistryRequest::new(
        key.clone(),
        identity.rhash,
        identity
            .sign_binding(&identity.rhash, &NodeRegistryType::Sentinel, &[NodeRegistryType::Sentinel])
            .unwrap(),
        vec![NodeRegistryType::Sentinel],
        NodeRegistryType::Sentinel,
    );
    let msg = Message {
        chain_id: "test".into(),
        action: "Register".into(),
        body: serialize_to_bytes_rmp(&req).unwrap(),
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&msg).unwrap();
    let result = sentinel.on_data_received(raw);
    assert!(result.is_err());
    match result.unwrap_err() {
        SentinelError::Registry(msg) => assert!(msg.contains("Insufficient stake")),
        _ => panic!("expected Registry error"),
    }
}

