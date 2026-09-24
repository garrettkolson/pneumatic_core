//! Registration tests: role selection (prefer/fallback/maxed-out/none),
//! find-by-key sets, multi-bucket registration, role-set auth, capacity
//! tracking + concurrent admission, and the directory-response trust gates.
use super::helpers::*;
use super::super::*;

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
