//! End-to-end: a generated node's own config chain produces a fragment the probe
//! accepts.
//!
//! This is the wiring test. The unit tests in `probe.rs` and `fragments.rs` cover
//! the judgment against synthetic inputs; nothing there proves that
//! `config.json`'s `mesh_fragment_path` reaches a running node, that the identity
//! signing a fragment is the one `manifest.json` lists, or that bucket names on
//! the writing side match the names the judge looks up. Each of those is a
//! plausible silent failure: the fragment lands somewhere nobody reads, verifies
//! under a key the manifest does not have, or arrives under `"sentinels"` where
//! the topology expects something else. Every one of them reports a healthy
//! cluster as unobserved, or the reverse.
//!
//! The chain here is the real one: `Config::build()` (the call both node binaries
//! make), a real `NodeRegistry`, `MeshFragment::dump_to`, then `Fragments::load`
//! and `evaluate`. `network = None`, so no sockets — a fragment is about directory
//! contents, which are decided before any transport exists.
//!
//! One test, deliberately: `Config::build()` reads `PNEUMATIC_CONFIG_FILE` /
//! `PNEUMATIC_ENV_DIR`, which are process-global. The same test then ages,
//! tampers with, and re-signs the fragments it wrote, because each of those needs
//! *valid* signed evidence and can only be built from the real thing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pneumatic_core::config::Config;
use pneumatic_core::node::registry::fragment::{bucket_key, MeshFragment};
use pneumatic_core::node::registry::{NodeRegistry, NullConnection};
use pneumatic_core::node::{NodeRegistryNode, NodeRegistryType};
use pneumatic_core::rns::identity::NodeIdentity;

use pneumatic_testnet_gen::fragments::{FragmentIssue, Fragments};
use pneumatic_testnet_gen::probe::{evaluate, Finding, ManifestIndex};
use pneumatic_testnet_gen::topology::{Mesh, Role};

fn env_template() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/config/env/env.json")
}

fn role_type(role: &str) -> NodeRegistryType {
    match role {
        "sentinel" => NodeRegistryType::Sentinel,
        "executor" => NodeRegistryType::Executor,
        "finalizer" => NodeRegistryType::Finalizer,
        "committer" => NodeRegistryType::Committer,
        other => panic!("unexpected role {other:?} in manifest"),
    }
}

fn rhash_of(hex_rhash: &str) -> [u8; 16] {
    let bytes = hex::decode(hex_rhash).expect("manifest rhash is hex");
    bytes.as_slice().try_into().expect("rhash is 16 bytes")
}

/// Per-node facts from the manifest, keyed by name — the manifest is the only
/// place `dir` lives, and it is what an operator would read too.
struct ManifestNodes(BTreeMap<String, serde_json::Value>);

impl ManifestNodes {
    fn new(manifest: &serde_json::Value) -> Self {
        ManifestNodes(
            manifest["nodes"]
                .as_array()
                .expect("nodes")
                .iter()
                .map(|n| (n["name"].as_str().unwrap().to_string(), n.clone()))
                .map(|(name, value)| {
                    // Every node must be locatable; a manifest without `dir` is a
                    // generator bug worth stopping on.
                    assert!(value["dir"].is_string(), "{name} has no dir in the manifest");
                    (name, value)
                })
                .collect(),
        )
    }

    fn dir(&self, name: &str) -> PathBuf {
        PathBuf::from(self.0[name]["dir"].as_str().expect("dir"))
    }

    fn fragment_path(&self, name: &str) -> PathBuf {
        self.dir(name).join("mesh_fragment.json")
    }

    fn rhash(&self, name: &str) -> String {
        self.0[name]["rhash_hex"].as_str().expect("rhash_hex").to_string()
    }

    fn role(&self, name: &str) -> String {
        self.0[name]["role"].as_str().expect("role").to_string()
    }

    fn peers(&self, name: &str) -> Vec<String> {
        self.0[name]["peers"]
            .as_array()
            .expect("peers")
            .iter()
            .map(|p| p.as_str().expect("peer name").to_string())
            .collect()
    }
}

/// Write every node's fragment by walking its own generated config, seeding each
/// registry with exactly the peers the generated topology gives it.
fn write_fragments_for_generated_cluster(cluster_dir: &Path) -> serde_json::Value {
    let raw = std::fs::read(cluster_dir.join("manifest.json")).expect("manifest");
    let manifest: serde_json::Value = serde_json::from_slice(&raw).expect("manifest json");
    let nodes = ManifestNodes::new(&manifest);

    for name in nodes.0.keys() {
        let node_dir = nodes.dir(name);

        // The real boot path for these values, overrides and all.
        std::env::set_var("PNEUMATIC_CONFIG_FILE", node_dir.join("config.json"));
        std::env::set_var("PNEUMATIC_ENV_DIR", node_dir.join("env"));
        let config = Arc::new(
            Config::build()
                .unwrap_or_else(|e| panic!("generated node {name} must boot its config: {}", e.message)),
        );

        // The config key has to survive into the built Config, or nothing else in
        // this chain is reachable.
        let fragment_path = config
            .mesh_fragment_path
            .clone()
            .unwrap_or_else(|| panic!("{name}: mesh_fragment_path must reach Config"));
        assert_eq!(
            fragment_path,
            nodes.fragment_path(name),
            "the fragment must land where the generator wrote the path"
        );

        let registry = Arc::new(NodeRegistry::init(
            Arc::clone(&config),
            None,
            Arc::new(|_, _| true),
        ));

        // Seed this node's peers, keyed by their Ed25519 key (the registry's map
        // key) and carrying their rhash (what a fragment reports).
        for peer in nodes.peers(name) {
            let key = hex::decode(
                nodes.0[&peer]["ed25519_public_key_hex"].as_str().expect("key"),
            )
            .expect("hex key");
            registry
                .get_nodes(&role_type(&nodes.role(&peer)))
                .unwrap_or_else(|| panic!("{name}: {} role must be installed", nodes.role(&peer)))
                .insert(
                    key,
                    NodeRegistryNode::new(
                        rhash_of(&nodes.rhash(&peer)),
                        Box::new(NullConnection),
                    ),
                );
        }

        MeshFragment::dump_to(&registry, &config.identity, &fragment_path)
            .unwrap_or_else(|e| panic!("{name}: writing a fragment must succeed: {e}"));
    }
    manifest
}

#[test]
fn a_generated_cluster_reports_itself_complete_and_its_evidence_holds_up() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut spec = pneumatic_testnet_gen::emit::GenSpec::new(dir.path().to_path_buf());
    spec.env_template = env_template();
    spec.counts = [
        (Role::Sentinel, 1),
        (Role::Executor, 1),
        (Role::Finalizer, 1),
        (Role::Committer, 1),
    ];
    pneumatic_testnet_gen::emit::generate(&spec).expect("generate");
    let manifest = write_fragments_for_generated_cluster(dir.path());

    let cluster = ManifestIndex::from_manifest(&manifest).expect("index");
    let mesh = Mesh::from_manifest(&manifest).expect("mesh");
    let nodes = ManifestNodes::new(&manifest);
    let node_count = mesh.plan.len();
    let all_names: Vec<String> = mesh.plan.iter().map(|p| p.node.name.clone()).collect();

    // ---------- 1. honest evidence judges clean ----------
    let fragments =
        Fragments::load(dir.path(), &cluster, 60, Fragments::now()).expect("load fresh");
    assert_eq!(
        fragments.files_seen, node_count,
        "only fragments may be counted: env.json, genesis.json and the keystore are \
         not evidence, and manifest.json is the probe's other input. issues: {:?}",
        fragments.issues
    );
    assert!(fragments.issues.is_empty(), "clean fragments: {:?}", fragments.issues);
    assert_eq!(fragments.snapshot.nodes.len(), node_count);

    let report = evaluate(&mesh, &fragments.snapshot, &cluster);
    assert!(
        report.is_complete(),
        "a cluster reporting exactly its topology must be complete, got: {:?}",
        report.findings.iter().map(|f| f.describe()).collect::<Vec<_>>()
    );
    // 4 nodes, full mesh: 3 directed edges each.
    assert_eq!(report.expected_edges, 12);
    assert_eq!(report.observed_edges, 12);
    assert_eq!(report.verified.len(), node_count);

    // ---------- 2. the same evidence, judged too late ----------
    // Age is provable because the timestamp is signed, so the only thing varied
    // here is the clock the judge compares against.
    let late =
        Fragments::load(dir.path(), &cluster, 60, Fragments::now() + 3_600).expect("load aged");
    assert!(late.snapshot.nodes.is_empty(), "stale fragments must not be used");
    assert_eq!(late.issues.len(), node_count, "one stale issue per node");
    assert!(
        late.issues
            .iter()
            .all(|i| matches!(i, FragmentIssue::Stale { .. })),
        "got {:?}",
        late.issues
    );
    assert!(
        !evaluate(&mesh, &late.snapshot, &cluster).is_complete(),
        "an all-stale fleet cannot be a clean run"
    );

    // ---------- 3. evidence edited after signing is refused ----------
    let victim = &all_names[0];
    let path = nodes.fragment_path(victim);
    let raw = std::fs::read(&path).expect("fragment");
    let mut edited: serde_json::Value = serde_json::from_slice(&raw).expect("fragment json");
    // Invent a peer in a bucket the node really has, and leave the signature alone.
    let bucket = edited["buckets"]
        .as_object_mut()
        .expect("buckets")
        .iter_mut()
        .find(|(_, peers)| !peers.as_array().map(Vec::is_empty).unwrap_or(true))
        .expect("a non-empty bucket")
        .0
        .clone();
    edited["buckets"][&bucket]
        .as_array_mut()
        .expect("bucket array")
        .push(serde_json::json!({
            "rhash_hex": "cd".repeat(16), "vouched": true, "last_seen_age_secs": 0
        }));
    std::fs::write(&path, serde_json::to_vec(&edited).expect("json")).expect("write");

    let tampered = Fragments::load(dir.path(), &cluster, 60, Fragments::now()).expect("load");
    assert_eq!(tampered.issues.len(), 1, "exactly the touched fragment is rejected");
    assert!(
        matches!(tampered.issues[0], FragmentIssue::BadSignature { .. }),
        "an edited bucket must surface as a bad signature, got {:?}",
        tampered.issues[0]
    );
    assert_eq!(tampered.snapshot.nodes.len(), node_count - 1);
    assert!(!evaluate(&mesh, &tampered.snapshot, &cluster).is_complete());

    // ---------- 4. a reporter the topology does not contain ----------
    let other = &all_names[1];
    let other_path = nodes.fragment_path(other);
    let raw_other = std::fs::read(&other_path).expect("fragment");
    let mut foreign: serde_json::Value = serde_json::from_slice(&raw_other).expect("fragment json");
    foreign["node"]["rhash_hex"] = serde_json::Value::String("ee".repeat(16));
    std::fs::write(&other_path, serde_json::to_vec(&foreign).expect("json")).expect("write");

    let foreign_fragments =
        Fragments::load(dir.path(), &cluster, 60, Fragments::now()).expect("load");
    assert!(
        foreign_fragments
            .issues
            .iter()
            .any(|i| matches!(i, FragmentIssue::UnknownReporter { .. })),
        "a fragment from outside the topology must be named: {:?}",
        foreign_fragments.issues
    );

    // ---------- 5. listed, and unreachable ----------
    // Rewrite that fragment as a properly signed one carrying real delivery
    // failures. Every directory-only check passes here: the peer is present in the
    // bucket. This is the state only a fragment can show.
    let identity =
        NodeIdentity::load_or_create(&nodes.dir(other).join("node_identity.json")).expect("keystore");
    let mut failing: MeshFragment =
        serde_json::from_slice(&raw_other).expect("the fragment this node wrote is well-formed");
    let peer = mesh
        .plan
        .iter()
        .find(|p| p.node.name == *other)
        .expect("plan entry")
        .peers[0];
    let peer_name = mesh.plan[peer].node.name.clone();
    failing.delivery_failures.insert(
        format!("{}/{}", bucket_key(&role_type(&nodes.role(&peer_name))), nodes.rhash(&peer_name)),
        42,
    );
    failing.sign_now(&identity).expect("re-sign as the node itself would");
    failing.write_atomically(&other_path).expect("write the re-signed fragment");

    // Put the tampered fragment back. Rejections must clear when the evidence is
    // valid again — a report that stays red after the operator fixes the file is a
    // report nobody trusts.
    std::fs::write(&path, &raw).expect("restore the untampered fragment");
    let with_failures =
        Fragments::load(dir.path(), &cluster, 60, Fragments::now()).expect("load");
    assert!(
        with_failures.issues.is_empty(),
        "a genuinely signed fragment is evidence even when it reports pain: {:?}",
        with_failures.issues
    );
    let mut reach = evaluate(&mesh, &with_failures.snapshot, &cluster);
    // With valid evidence everywhere, the mesh itself is intact — the only problem
    // left is the one only a fragment can reveal.
    let before = reach.findings.len();
    assert_eq!(before, 0, "the mesh must be clean before reachability is folded in");
    reach.add_reachability(with_failures.reachability(&mesh, &cluster, 10));
    assert_eq!(reach.findings.len(), before + 1, "exactly one unreachable peer");
    assert!(
        reach.findings.iter().any(|f| matches!(
            f,
            Finding::Unreachable { node, failures: 42, .. } if node == other
        )),
        "the finding must name the node and its count"
    );
    assert!(
        !reach.verified.contains(other),
        "a node that cannot reach its peers must not be listed as verified"
    );
    assert!(!reach.is_complete());

    // Below the threshold it is not a finding — boot transients are normal, and a
    // report that cries wolf gets ignored.
    let quiet =
        Fragments::load(dir.path(), &cluster, 60, Fragments::now()).expect("load");
    let mut thresholded = evaluate(&mesh, &quiet.snapshot, &cluster);
    thresholded.add_reachability(quiet.reachability(&mesh, &cluster, 100));
    assert_eq!(thresholded.findings.len(), before, "42 failures must not fire at threshold 100");
}

/// The two naming conventions must agree, or every peer on a healthy cluster is
/// reported missing. Core writes bucket names; the generator's roles define the
/// expectations the judge looks up.
#[test]
fn core_bucket_names_match_the_generators_role_plurals() {
    assert_eq!(bucket_key(&NodeRegistryType::Sentinel), Role::Sentinel.plural());
    assert_eq!(bucket_key(&NodeRegistryType::Executor), Role::Executor.plural());
    assert_eq!(bucket_key(&NodeRegistryType::Finalizer), Role::Finalizer.plural());
    assert_eq!(bucket_key(&NodeRegistryType::Committer), Role::Committer.plural());
    // Archiver has no generator role, but a full node maintains the bucket, so the
    // probe must not be surprised to find it in a fragment.
    assert_eq!(bucket_key(&NodeRegistryType::Archiver), "archivers");
}
