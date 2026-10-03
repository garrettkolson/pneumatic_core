//! Validation of generated artifacts against the **real** loaders.
//!
//! A generator's only contract is "what I wrote is what the consumer parses", so
//! these tests do not compare against expected JSON shapes the test itself
//! defines — that would just restate the emitter. Each test hands a generated
//! file to the same type the node binaries use:
//!
//! | Artifact | Real consumer |
//! |---|---|
//! | `config.json` | `pneumatic_core::config::ConfigSpec` (the serde shape `Config::build` parses) |
//! | `env/env.json` | `EnvironmentMetadataSpec` → `EnvironmentMetadata::load_from_spec` |
//! | `node_identity.json` | `NodeIdentity::load_or_create` on an existing file, i.e. the loader's `load` half |
//! | peer/port matrix | cross-checked between two nodes' own configs |
//!
//! The peer/port test is the one that earns this file's keep: the j-rule is the
//! single place where getting one integer wrong produces a cluster that boots,
//! peers with nobody, and reports nothing worse than "no live route yet".

use std::path::{Path, PathBuf};

use pneumatic_core::config::ConfigSpec;
use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
use pneumatic_core::crypto::AsymCryptoProvider as _;
use pneumatic_core::rns::identity::NodeIdentity;
use serde_json::Value;

use pneumatic_testnet_gen::emit::{generate, GenSpec};
use pneumatic_testnet_gen::topology::TopologyMode;

/// The repo's own env template, so the test exercises the file operators use.
fn env_template() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/config/env/env.json")
}

fn spec_for(out: &Path) -> GenSpec {
    let mut spec = GenSpec::new(out.to_path_buf());
    spec.env_template = env_template();
    spec.counts = [
        (pneumatic_testnet_gen::topology::Role::Sentinel, 2),
        (pneumatic_testnet_gen::topology::Role::Executor, 2),
        (pneumatic_testnet_gen::topology::Role::Finalizer, 2),
        (pneumatic_testnet_gen::topology::Role::Committer, 2),
    ];
    spec
}

/// Read back the generated manifest — the generator's own index of what it made.
fn manifest(out: &Path) -> Value {
    let raw = std::fs::read(out.join("manifest.json")).expect("manifest.json should exist");
    serde_json::from_slice(&raw).expect("manifest.json should parse")
}

fn node_dirs(out: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(out.join("nodes"))
        .expect("nodes/ should exist")
        .map(|entry| entry.expect("dir entry").path())
        .collect();
    dirs.sort();
    dirs
}

#[test]
fn every_generated_config_parses_as_the_real_config_spec() {
    let dir = tempfile::tempdir().expect("tempdir");
    let report = generate(&spec_for(dir.path())).expect("generate");
    assert_eq!(report.nodes.len(), 8);

    for node_dir in node_dirs(dir.path()) {
        let path = node_dir.join("config.json");
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        // The parse *is* the assertion: `ConfigSpec` is the type `Config::build`
        // deserializes, so reaching the next line means a node would accept this
        // file instead of failing at boot. Its fields are private by design, so
        // values are checked from the JSON below rather than re-derived here.
        let _spec: ConfigSpec = serde_json::from_slice(&raw)
            .unwrap_or_else(|e| panic!("{} must parse as ConfigSpec: {e}", path.display()));
        let value: Value = serde_json::from_slice(&raw).expect("value");
        assert!(
            value["identity_path"].as_str().unwrap().ends_with("node_identity.json"),
            "{} must point at its own keystore",
            path.display()
        );
        assert_eq!(value["main_env_id"], serde_json::json!("env"));
        assert_eq!(
            value["bootstrap_peers"].as_array().unwrap().len(),
            report
                .nodes
                .iter()
                .find(|n| n.name == node_dir.file_name().unwrap().to_str().unwrap())
                .expect("node in report")
                .interfaces,
            "bootstrap_peers must have one entry per bound interface"
        );
    }
}

#[test]
fn every_env_dir_loads_through_the_real_environment_loader() {
    let dir = tempfile::tempdir().expect("tempdir");
    generate(&spec_for(dir.path())).expect("generate");

    let mut log_files = std::collections::BTreeSet::new();
    for node_dir in node_dirs(dir.path()) {
        let path = node_dir.join("env/env.json");
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let spec: EnvironmentMetadataSpec = serde_json::from_slice(&raw)
            .unwrap_or_else(|e| panic!("{} must parse: {e}", path.display()));
        EnvironmentMetadata::load_from_spec(spec)
            .unwrap_or_else(|e| panic!("{} must build metadata: {e}", path.display()));

        let value: Value = serde_json::from_slice(&raw).expect("value");
        // One log file per node. N nodes appending to one path is unreadable mush.
        let log = value["log_file"].as_str().expect("log_file").to_string();
        assert!(
            log.starts_with(node_dir.to_str().unwrap()),
            "{}'s log must live in its own directory, got {log}",
            node_dir.display()
        );
        log_files.insert(log);
    }
    assert_eq!(log_files.len(), 8, "log files must be distinct per node");
}

/// The keystore is the one artifact that must survive a restart byte-identically:
/// reloading it has to yield the *same* identity, including the hybrid PQC keys,
/// which are the persisted identity rather than something derived from a seed.
#[test]
fn every_keystore_reloads_as_the_same_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let report = generate(&spec_for(dir.path())).expect("generate");

    for node in &report.nodes {
        let path = Path::new(&node.dir).join("node_identity.json");
        let reloaded = NodeIdentity::load_or_create(&path)
            .unwrap_or_else(|e| panic!("reload {}: {e}", path.display()));
        assert_eq!(
            hex::encode(reloaded.rhash),
            node.rhash_hex,
            "{}: rhash must be stable across reload",
            node.name
        );
        assert_eq!(
            hex::encode(reloaded.ed25519.public_key().expect("ed25519 key")),
            node.ed25519_public_key_hex,
            "{}: ed25519 key must be stable across reload",
            node.name
        );
        assert_eq!(
            hex::encode(reloaded.rns.get_public_key().expect("rns key")),
            node.rns_public_key_hex,
            "{}: rns key must be stable across reload",
            node.name
        );
    }
}

/// The j-rule, checked from the artifacts alone and in **both** directions:
/// every `bootstrap_peers` port a node was told to forward to must be the port
/// the target node actually listens on for that node — the target's base port
/// plus the forwarder's index in the target's own peer list.
///
/// Written because the emitter had this backwards once: it computed the port the
/// peer uses to reach *us* and put it in *our* config. Every node then booted,
/// every config looked populated, and no link ever handshook.
#[test]
fn bootstrap_peer_ports_are_the_targets_listen_ports() {
    let dir = tempfile::tempdir().expect("tempdir");
    generate(&spec_for(dir.path())).expect("generate");
    let manifest = manifest(dir.path());
    let nodes = manifest["nodes"].as_array().expect("nodes");

    // index by rns key so a bootstrap entry can be resolved to the node it names
    let by_rns: std::collections::HashMap<String, &Value> = nodes
        .iter()
        .map(|n| (n["rns_public_key_hex"].as_str().unwrap().to_string(), n))
        .collect();

    for node in nodes {
        let name = node["name"].as_str().unwrap();
        let path = Path::new(node["dir"].as_str().unwrap()).join("config.json");
        let config: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("config.json")).expect("parse");

        let peers = config["bootstrap_peers"].as_array().expect("bootstrap_peers");
        assert_eq!(peers.len(), node["interfaces"].as_u64().unwrap() as usize);

        for peer in peers {
            let peer_key = peer["public_key"].as_str().expect("peer public_key");
            let port = peer["port"].as_u64().expect("peer port");
            assert_eq!(
                peer_key.len(),
                128,
                "bootstrap_peers carry the 64-byte RNS key in hex; a 32-byte Ed25519 \
                 key here is the trap the runbook documents"
            );
            let target = *by_rns
                .get(peer_key)
                .unwrap_or_else(|| panic!("{name}: bootstrap peer key is not a generated node"));
            let target_name = target["name"].as_str().unwrap();

            // What the target must listen on for `name`, read from the target's
            // own config — not from the generator's internals.
            let target_path = Path::new(target["dir"].as_str().unwrap()).join("config.json");
            let target_config: Value =
                serde_json::from_slice(&std::fs::read(&target_path).expect("config")).expect("parse");
            let target_base = target_config["rns_port"].as_u64().expect("rns_port");
            let target_peers: Vec<&str> = target_config["bootstrap_peers"]
                .as_array()
                .expect("target bootstrap_peers")
                .iter()
                .map(|p| p["public_key"].as_str().unwrap())
                .collect();
            let own_key = node["rns_public_key_hex"].as_str().unwrap();
            let j = target_peers
                .iter()
                .position(|k| *k == own_key)
                .unwrap_or_else(|| panic!("{name} is absent from {target_name}'s peers: not symmetric"));

            assert_eq!(
                port,
                target_base + j as u64,
                "{name} must forward to {target_name} on base {target_base} + j {j}"
            );
            // Sanity: the target really binds that port.
            assert!(
                port < target_base + target_peers.len() as u64,
                "{name}→{target_name}: port {port} is outside the target's interface range"
            );
        }
    }
}

/// The trap in the runbook: genesis judges a node by its Ed25519 key while
/// configs carry its RNS key, and both are 64-byte hex blobs in the same
/// manifest. Getting this backwards yields a node that boots, installs no roles,
/// and rejects registrations.
#[test]
fn genesis_is_keyed_by_ed25519_keys_and_never_by_rns_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    generate(&spec_for(dir.path())).expect("generate");
    let manifest = manifest(dir.path());
    let genesis: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("genesis.json")).expect("genesis.json"),
    )
    .expect("genesis parses");

    let rns_keys: std::collections::HashSet<&str> = manifest["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["rns_public_key_hex"].as_str().unwrap())
        .collect();

    let genesis_nodes = genesis["nodes"].as_array().expect("genesis nodes");
    assert_eq!(genesis_nodes.len(), 8);
    for (node, manifest_node) in genesis_nodes.iter().zip(manifest["nodes"].as_array().unwrap()) {
        let key = node["public_key_hex"].as_str().expect("public_key_hex");
        assert_eq!(key, manifest_node["ed25519_public_key_hex"].as_str().unwrap());
        assert!(
            !rns_keys.contains(key),
            "genesis listed an RNS key as a validator; the node would boot with no roles"
        );
        assert_eq!(node["stake"], serde_json::json!(1000));
    }

    // Epochs 0 and 1 both, and the recency window copied from the env template.
    assert_eq!(genesis["stake_snapshot_epochs"], serde_json::json!([0, 1]));
    let template: Value = serde_json::from_slice(
        &std::fs::read(env_template()).expect("read env template"),
    )
    .expect("template parses");
    let expected_recency = template["shielded_root_recency"].as_u64().unwrap_or(10);
    assert_eq!(genesis["shielded_root_recency"], serde_json::json!(expected_recency));
}

/// Re-running must not mint new identities — that would silently orphan the
/// stake genesis put under the old keys — while configs stay in sync.
#[test]
fn re_running_preserves_keys_and_rewrites_configs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = generate(&spec_for(dir.path())).expect("first generate");
    assert_eq!(first.keystore_created, 8);
    assert_eq!(first.keystore_reused, 0);

    let second = generate(&spec_for(dir.path())).expect("second generate");
    assert_eq!(second.keystore_created, 0, "re-run must not create keystores");
    assert_eq!(second.keystore_reused, 8);
    let before: Vec<&str> = first.nodes.iter().map(|n| n.ed25519_public_key_hex.as_str()).collect();
    let after: Vec<&str> = second.nodes.iter().map(|n| n.ed25519_public_key_hex.as_str()).collect();
    assert_eq!(before, after, "identities must survive a re-run");
}

/// `--force-keys` is the explicit escape hatch, and it must actually replace.
#[test]
fn force_keys_replaces_identities() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = generate(&spec_for(dir.path())).expect("first");
    let mut again = spec_for(dir.path());
    again.force_keys = true;
    let second = generate(&again).expect("second with force_keys");
    assert_eq!(second.keystore_created, 8);
    assert_ne!(
        first.nodes[0].ed25519_public_key_hex,
        second.nodes[0].ed25519_public_key_hex,
        "--force-keys must mint a new identity"
    );
}

/// The role graph must reach the emitted peer lists, not just the in-memory mesh.
#[test]
fn role_graph_mode_omits_executor_to_executor_peers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut spec = spec_for(dir.path());
    spec.mode = TopologyMode::RoleGraph;
    let report = generate(&spec).expect("generate");

    for node in report.nodes.iter().filter(|n| node_role(&manifest(dir.path()), n).as_deref() == Some("executor")) {
        let peers = report
            .nodes
            .iter()
            .find(|n| n.name == node.name)
            .unwrap()
            .peers
            .clone();
        assert!(
            !peers.iter().any(|p| p.starts_with("executor-")),
            "{} (executor) must not peer with executors in role-graph mode: {peers:?}",
            node.name
        );
    }
}

fn node_role(manifest: &Value, node: &pneumatic_testnet_gen::emit::NodeReport) -> Option<String> {
    manifest["nodes"]
        .as_array()?
        .iter()
        .find(|n| n["name"].as_str() == Some(node.name.as_str()))
        .and_then(|n| n["role"].as_str().map(|r| r.to_string()))
}
