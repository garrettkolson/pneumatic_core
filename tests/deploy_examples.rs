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
