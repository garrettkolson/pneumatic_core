//! Heartbeat tests: forged heartbeats do not refresh `last_seen`,
//! authenticated ones do, and the binding is tuple-specific.
use super::helpers::*;
use super::super::*;

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
