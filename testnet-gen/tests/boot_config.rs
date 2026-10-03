//! Does a generated node's configuration actually load, end to end?
//!
//! `generation.rs` checks each artifact against its own parser. This checks the
//! *chain*: `Config::build()` — the exact call both node binaries make at boot —
//! against a generated node directory, resolving `config.json` and the env dir
//! through the `PNEUMATIC_CONFIG_FILE` / `PNEUMATIC_ENV_DIR` overrides and
//! loading the keystore behind it.
//!
//! Separate test binary on purpose: `set_var` and `Config::build` are
//! process-global, and this file contains one test so nothing else in the binary
//! can observe the variables.

use std::path::PathBuf;

use pneumatic_core::config::Config;
use pneumatic_core::crypto::AsymCryptoProvider as _;

use pneumatic_testnet_gen::emit::GenSpec;
use pneumatic_testnet_gen::topology::Role;

#[test]
fn a_generated_node_boots_its_config_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut spec = GenSpec::new(dir.path().to_path_buf());
    spec.env_template = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/config/env/env.json");
    spec.counts = [
        (Role::Sentinel, 1),
        (Role::Executor, 1),
        (Role::Finalizer, 1),
        (Role::Committer, 1),
    ];
    let report = pneumatic_testnet_gen::emit::generate(&spec).expect("generate");

    let node = &report.nodes[0];
    let node_dir = std::path::Path::new(&node.dir);

    // Absolute overrides, which is the point of them: no CWD change, so this
    // cannot perturb any other test in the process.
    std::env::set_var("PNEUMATIC_CONFIG_FILE", node_dir.join("config.json"));
    std::env::set_var("PNEUMATIC_ENV_DIR", node_dir.join("env"));

    let config = Config::build().unwrap_or_else(|e| {
        panic!(
            "generated node {} must boot its config chain: {}",
            node.name, e.message
        )
    });

    // The values the operator asked for survived the whole load path.
    assert_eq!(
        config.rns_port, node.rns_port,
        "rns_port must come from the generated config, not the 4242 default"
    );
    assert_eq!(
        config.bootstrap_peers.len(),
        node.interfaces,
        "generated bootstrap peers must reach Config"
    );
    // The keystore behind identity_path is the one genesis was written for.
    let public_key = hex::encode(
        config
            .identity
            .ed25519
            .public_key()
            .expect("identity public key"),
    );
    assert_eq!(
        public_key, node.ed25519_public_key_hex,
        "Config loaded a different identity than genesis was keyed by — the node \
         would boot and install no roles"
    );
    assert_eq!(
        hex::encode(config.rhash),
        node.rhash_hex,
        "rhash must match the manifest"
    );
    // A full node has a bucket for all five registry types. This is the set of
    // buckets that *exist*, not the set the node advertises: the composite
    // narrows what it announces to its installed roles via `declared_roles`,
    // because a peer admits a node under every type it declares.
    assert_eq!(
        config.node_registry_types.len(),
        5,
        "a full node must have all five buckets (four roles + Archiver)"
    );
}
