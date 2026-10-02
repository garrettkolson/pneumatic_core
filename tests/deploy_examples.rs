//! Deploy-example guard (Phase 8): the files under `deploy/config/` are the
//! operator-facing starting point (and the Docker/compose mounts depend on
//! their shape), so they must stay parseable and valid against the very
//! structs the binaries load — a schema drift fails this test, not someone's
//! `docker compose up`.

use std::path::{Path, PathBuf};
use std::{fs,};

use pneumatic_core::config::ConfigSpec;
use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

#[test]
fn deploy_config_examples_parse_as_config_spec() {
    for rel in [
        "deploy/config/committer/config.json",
        "deploy/config/full-node/config.json",
    ] {
        let path = repo_root().join(rel);
        let raw = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_value::<ConfigSpec>(
            serde_json::from_slice(&raw).expect("config.json is valid JSON"),
        )
        .unwrap_or_else(|e| panic!("{rel} must parse as ConfigSpec: {e}"));
    }
}

#[test]
fn deploy_env_example_parses_validates_and_loads() {
    let path = repo_root().join("deploy/config/env/env.json");
    let raw = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let value: serde_json::Value =
        serde_json::from_slice(&raw).expect("env.json is valid JSON");

    // Parse + validate exactly as Config::get_environment_metadata does.
    let spec: EnvironmentMetadataSpec =
        serde_json::from_value(value.clone()).expect("env.json must parse");
    spec.validate().expect("env.json must pass spec validation");

    // Full load (crypto provider, cost model, logger wiring). The in-file
    // `log_file` is a container path (`/pneumatic/...`) a CI user cannot
    // create, so substitute a temp path for the load; everything else is
    // loaded verbatim.
    let tmp = tempfile::tempdir().expect("tempdir");
    let log_path = tmp.path().join("pneumatic.log");
    let mut patched = value;
    patched["log_file"] = serde_json::Value::String(log_path.display().to_string());
    let patched_spec: EnvironmentMetadataSpec =
        serde_json::from_value(patched).expect("patched env.json must parse");
    let env = EnvironmentMetadata::load_from_spec(patched_spec)
        .expect("env.json must load into EnvironmentMetadata");
    assert_eq!(env.environment_id, "env");
}

#[test]
fn deploy_genesis_example_parses_and_agrees_with_the_env_example() {
    // The genesis template is the input to `pneumatic_data_service --genesis`,
    // so it must keep parsing as `GenesisSpec` — and it must AGREE with
    // `deploy/config/env/env.json`, because a genesis whose `environment_id` or
    // recency window disagrees with the env spec produces a cluster that boots
    // and then fails on its first transaction rather than at startup.
    let raw = fs::read(repo_root().join("deploy/config/testnet/genesis.example.json"))
        .expect("read deploy/config/testnet/genesis.example.json");
    let spec: pneumatic_data_service::GenesisSpec =
        serde_json::from_slice(&raw).expect("genesis.example.json must parse as GenesisSpec");

    // The shipped template is a template: placeholder keys, by design.
    assert_eq!(spec.nodes.len(), 2, "the template documents a two-node cluster");
    assert!(
        spec.nodes.iter().all(|n| n.public_key_hex.starts_with("REPLACE")),
        "the template must ship placeholders, never a key that could boot a real node"
    );

    // Cross-file agreement with the env example.
    let env_raw = fs::read(repo_root().join("deploy/config/env/env.json")).expect("read env.json");
    let env_value: serde_json::Value = serde_json::from_slice(&env_raw).expect("env.json parses");
    assert_eq!(
        spec.environment_id,
        env_value["environment_id"].as_str().expect("env id is a string"),
        "genesis environment_id must equal the env spec's environment_id"
    );
    assert_eq!(
        spec.shielded_root_recency, 10,
        "the template's recency window must match the value the pool is built with"
    );

    // Both boot-critical seeds on, and both epochs the nodes read are covered.
    assert!(spec.seed_shielded_pool, "a composite cannot boot without a seeded pool");
    assert!(spec.seed_partition_token, "the sentinel's chain-tip lookup needs the partition token");
    assert!(
        spec.stake_snapshot_epochs.contains(&0) && spec.stake_snapshot_epochs.contains(&1),
        "epoch 1 is the boot read and epoch 0 is the pipeline-path read"
    );
}
