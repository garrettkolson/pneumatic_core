//! Tests for mesh fragments (`fragment.rs`) — the signed self-report a node
//! writes for `mesh-probe` to aggregate.
//!
//! The properties worth pinning are the ones a wrong implementation would still
//! look correct about: a fragment that always verifies, a timestamp that can be
//! refreshed, or a dump whose byte form shifts between two reads of identical
//! state. Each of those fails quietly in production — the first as a monitoring
//! system that trusts forgeries, the second as a stale cluster reporting green,
//! the third as a signature that breaks at random on a node whose DashMap
//! happened to iterate differently.

use super::helpers::*;
use super::super::fragment::{bucket_key, MeshFragment, FRAGMENT_VERSION};
use super::super::NullConnection;
use super::super::NodeRegistry;
use crate::crypto::AsymCryptoProvider as _;
use crate::node::{NodeRegistryNode, NodeRegistryType};
use crate::rns::identity::NodeIdentity;

fn key_of(identity: &NodeIdentity) -> Vec<u8> {
    identity.ed25519.public_key().expect("ed25519 public key")
}

/// Seed a peer the way a directory response would: present in the bucket, no
/// binding of our own for it.
fn seed_unvouched(reg: &NodeRegistry, identity: &NodeIdentity, node_type: &NodeRegistryType) {
    reg.get_nodes(node_type)
        .expect("type configured in this fixture")
        .insert(
            key_of(identity),
            NodeRegistryNode::new(identity.rhash, Box::new(NullConnection)),
        );
}

#[test]
fn a_dump_reports_the_peers_this_node_holds_and_verifies_under_its_own_key() {
    let host = test_identity();
    let peer = test_identity();
    let reg = registry_for(
        &host,
        &[(NodeRegistryType::Finalizer, 5), (NodeRegistryType::Executor, 5)],
        &[NodeRegistryType::Sentinel],
        vec![],
    );
    register_multi_bucket(&reg, &peer, NodeRegistryType::Finalizer, vec![
        NodeRegistryType::Finalizer,
    ]);

    let fragment = MeshFragment::from_registry(&reg, &host);

    assert_eq!(fragment.version, FRAGMENT_VERSION);
    assert_eq!(
        hex::encode(fragment.node.rhash_hex.as_bytes()),
        hex::encode(hex::encode(host.rhash).as_bytes()),
        "the fragment must carry our own rhash"
    );
    assert_eq!(fragment.peer_count(), 1);
    let finalizers = fragment.buckets.get("finalizers").expect("finalizers bucket");
    assert_eq!(finalizers.len(), 1);
    assert_eq!(finalizers[0].rhash_hex, hex::encode(peer.rhash));
    assert!(finalizers[0].vouched, "a directly-registered peer is vouched for");
    // Empty buckets are present, not omitted: "0 executors" is information, and a
    // probe that cannot tell "empty" from "not reported" cannot report the
    // difference either.
    assert!(fragment.buckets.contains_key("executors"));

    let mut signed = fragment.clone();
    signed.sign_now(&host).expect("sign");
    assert!(signed.verify(&key_of(&host)), "a clean fragment verifies");
    assert!(!signed.signature.is_empty());
}

/// The distinction the whole design rests on: being listed is not the same as
/// having met the node. A cluster that learned peers only from directory
/// responses has a full bucket and no direct links — and leaves cannot route
/// through each other, so that cluster does not work.
#[test]
fn a_peer_learned_from_a_directory_response_is_listed_but_not_vouched() {
    let host = test_identity();
    let learned = test_identity();
    let registered = test_identity();
    let reg = registry_for(
        &host,
        &[(NodeRegistryType::Finalizer, 5)],
        &[NodeRegistryType::Sentinel],
        vec![],
    );
    seed_unvouched(&reg, &learned, &NodeRegistryType::Finalizer);
    register_multi_bucket(&reg, &registered, NodeRegistryType::Finalizer, vec![
        NodeRegistryType::Finalizer,
    ]);

    let fragment = MeshFragment::from_registry(&reg, &host);
    let finalizers = fragment.buckets.get("finalizers").expect("bucket");
    assert_eq!(finalizers.len(), 2);

    let by_rhash: std::collections::BTreeMap<String, bool> = finalizers
        .iter()
        .map(|p| (p.rhash_hex.clone(), p.vouched))
        .collect();
    assert_eq!(by_rhash.get(&hex::encode(learned.rhash)), Some(&false));
    assert_eq!(by_rhash.get(&hex::encode(registered.rhash)), Some(&true));
}

#[test]
fn tampering_with_a_bucket_breaks_the_signature() {
    let host = test_identity();
    let reg = registry_for(
        &host,
        &[(NodeRegistryType::Finalizer, 5)],
        &[NodeRegistryType::Sentinel],
        vec![],
    );
    let mut fragment = MeshFragment::from_registry(&reg, &host);
    fragment.sign_now(&host).expect("sign");

    // An operator (or a log-shipping agent with write access) inventing a peer to
    // make a dashboard go green.
    fragment
        .buckets
        .entry("finalizers".to_string())
        .or_default()
        .push(crate::node::registry::fragment::FragmentPeer {
            rhash_hex: "ab".repeat(16),
            vouched: true,
            last_seen_age_secs: 0,
        });

    assert!(
        !fragment.verify(&key_of(&host)),
        "an edited bucket must not verify"
    );
}

/// The replay property. Staleness is only a meaningful check if the timestamp is
/// attested: otherwise anything in the pipeline can re-date an old fragment and
/// the age bound in `mesh-probe` becomes decorative.
#[test]
fn a_refreshed_timestamp_does_not_revive_a_stale_fragment() {
    let host = test_identity();
    let reg = registry_for(
        &host,
        &[(NodeRegistryType::Finalizer, 5)],
        &[NodeRegistryType::Sentinel],
        vec![],
    );
    let mut fragment = MeshFragment::from_registry(&reg, &host);
    fragment.sign_now(&host).expect("sign");
    let original_age = fragment.written_at_unix;

    fragment.written_at_unix += 3_600;
    assert!(
        !fragment.verify(&key_of(&host)),
        "re-stamping a signed fragment must invalidate it"
    );

    // And the stamp was real, not a zero left behind by the builder.
    let fresh = MeshFragment::from_registry(&reg, &host);
    assert_eq!(fresh.written_at_unix, 0, "unsigned fragments carry no attested time");
    assert!(original_age > 0);
}

/// Verification is against the key the *consumer* expects. Verifying against the
/// embedded key would accept a fragment that claims to be someone else — and the
/// buckets would then be attributed to the wrong node in the report.
#[test]
fn verification_uses_the_callers_key_not_the_one_the_fragment_claims() {
    let host = test_identity();
    let someone_else = test_identity();
    let reg = registry_for(
        &host,
        &[(NodeRegistryType::Finalizer, 5)],
        &[NodeRegistryType::Sentinel],
        vec![],
    );
    let mut fragment = MeshFragment::from_registry(&reg, &host);
    fragment.sign_now(&host).expect("sign");

    assert!(fragment.verify(&key_of(&host)));
    assert!(!fragment.verify(&key_of(&someone_else)));
    // Stronger than "the consumer's key decides": the claimed key is *inside* the
    // signed payload, so it cannot be substituted at all. Editing it invalidates
    // the signature under every key — the fragment's self-description is attested,
    // not merely reported. (The probe additionally cross-checks the claim against
    // `manifest.json`, because a valid signature by an unexpected node is still a
    // report from a node the topology does not contain.)
    let mut impersonating = fragment.clone();
    impersonating.node.ed25519_public_key_hex = hex::encode(key_of(&someone_else));
    assert!(
        !impersonating.verify(&key_of(&host)),
        "rewriting the claimed identity must invalidate the signature"
    );
    assert!(
        !impersonating.verify(&key_of(&someone_else)),
        "and it must not start verifying under the impersonated key"
    );
}

#[test]
fn an_unsigned_fragment_never_reaches_disk() {
    let host = test_identity();
    let reg = registry_for(
        &host,
        &[(NodeRegistryType::Finalizer, 5)],
        &[NodeRegistryType::Sentinel],
        vec![],
    );
    let fragment = MeshFragment::from_registry(&reg, &host);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fragment.json");

    let err = fragment
        .write_atomically(&path)
        .expect_err("unsigned fragments must be refused");
    assert!(format!("{err:?}").contains("unsigned"), "got: {err:?}");
    assert!(!path.exists(), "nothing may be left behind");
    // The parent directory is created for a node whose reporting dir does not
    // exist yet — a boot race must not be a permanent observability outage.
    let nested = dir.path().join("nodes/full/fragment.json");
    let mut signed = fragment;
    signed.sign_now(&host).expect("sign");
    signed.write_atomically(&nested).expect("write");
    assert!(nested.exists());
}

/// Two reads of an unchanged state must produce identical signed bytes.
///
/// `DashMap` iteration order is not guaranteed, so without sorting the peers the
/// payload would differ between two dumps of the same registry — and every
/// consumer would see a cluster flapping between two "different" states that are
/// byte-for-byte the same mesh.
#[test]
fn two_dumps_of_one_state_agree_byte_for_byte() {
    let host = test_identity();
    let reg = registry_for(
        &host,
        // Room for every peer we seed: a bucket at capacity silently drops
        // registrations, which would make this test compare 5 peers against 12.
        &[(NodeRegistryType::Finalizer, 20)],
        &[NodeRegistryType::Sentinel],
        vec![],
    );
    let mut peers = Vec::new();
    for _ in 0..12 {
        let peer = test_identity();
        register_multi_bucket(&reg, &peer, NodeRegistryType::Finalizer, vec![
            NodeRegistryType::Finalizer,
        ]);
        peers.push(peer);
    }

    let first = MeshFragment::from_registry(&reg, &host)
        .signing_payload()
        .expect("payload");
    for _ in 0..5 {
        let again = MeshFragment::from_registry(&reg, &host)
            .signing_payload()
            .expect("payload");
        assert_eq!(first, again, "payload must not depend on map iteration order");
    }
    assert_eq!(
        MeshFragment::from_registry(&reg, &host).peer_count(),
        peers.len()
    );
}

#[test]
fn bucket_names_are_the_ones_the_probe_expects() {
    // `testnet-gen/tests/fragments.rs` asserts these match `Role::plural()`.
    // Renaming either side without the other silently reports every peer as
    // missing, because the probe looks up expectations by bucket name.
    assert_eq!(bucket_key(&NodeRegistryType::Sentinel), "sentinels");
    assert_eq!(bucket_key(&NodeRegistryType::Executor), "executors");
    assert_eq!(bucket_key(&NodeRegistryType::Finalizer), "finalizers");
    assert_eq!(bucket_key(&NodeRegistryType::Committer), "committers");
    assert_eq!(bucket_key(&NodeRegistryType::Archiver), "archivers");
}
