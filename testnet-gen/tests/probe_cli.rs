//! End-to-end tests for the `mesh-probe` binary.
//!
//! The probe's whole job is to refuse to say "healthy" when it doesn't know, so
//! these test exit codes and classification rather than prose: the failure mode
//! this guards against is a deployment reading a green probe on a fleet that
//! never formed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use pneumatic_testnet_gen::probe::{NodeSnapshot, Snapshot};

const GEN: &str = env!("CARGO_BIN_EXE_pneumatic_testnet_gen");
const PROBE: &str = env!("CARGO_BIN_EXE_mesh-probe");

fn env_template() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../deploy/config/env/env.json")
        .to_string_lossy()
        .to_string()
}

/// Generate a cluster with the real generator, so the manifest under test is
/// byte-for-byte what an operator would have.
fn generate(out: &Path) -> serde_json::Value {
    let status = Command::new(GEN)
        .args([
            "--out",
            out.to_str().unwrap(),
            "--validators",
            "4",
            "--env-template",
            &env_template(),
        ])
        .output()
        .expect("generator should run");
    assert!(status.status.success(), "generator failed");
    let raw = std::fs::read(out.join("manifest.json")).expect("manifest");
    serde_json::from_slice(&raw).expect("manifest parses")
}

/// Build the snapshot every node *would* report if the mesh formed exactly as
/// generated.
fn complete_snapshot(manifest: &serde_json::Value) -> Snapshot {
    let nodes = manifest["nodes"].as_array().expect("nodes");
    let role_of: BTreeMap<String, String> = nodes
        .iter()
        .map(|n| {
            (
                n["name"].as_str().unwrap().to_string(),
                n["role"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let rhash_of: BTreeMap<String, String> = nodes
        .iter()
        .map(|n| {
            (
                n["name"].as_str().unwrap().to_string(),
                n["rhash_hex"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    let reported = nodes
        .iter()
        .map(|n| {
            let mut buckets: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for peer in n["peers"].as_array().expect("peers") {
                let name = peer.as_str().expect("peer name");
                buckets
                    .entry(format!("{}s", role_of[name]))
                    .or_default()
                    .push(rhash_of[name].clone());
            }
            NodeSnapshot {
                name: n["name"].as_str().unwrap().to_string(),
                rhash_hex: n["rhash_hex"].as_str().unwrap().to_string(),
                buckets,
            }
        })
        .collect();
    Snapshot { collected_at: Some("test".to_string()), nodes: reported }
}

fn write_snapshot(dir: &Path, name: &str, snapshot: &Snapshot) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_vec(snapshot).expect("serialize")).expect("write");
    path
}

fn probe(manifest: &Path, snapshot: &Path, json: bool) -> (i32, String) {
    let mut cmd = Command::new(PROBE);
    cmd.args(["--manifest", manifest.to_str().unwrap(), "--snapshot", snapshot.to_str().unwrap()]);
    if json {
        cmd.arg("--json");
    }
    let out = cmd.output().expect("probe should run");
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn a_healthy_cluster_exits_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = generate(dir.path());
    let snapshot = write_snapshot(dir.path(), "ok.json", &complete_snapshot(&manifest));

    let (code, text) = probe(&dir.path().join("manifest.json"), &snapshot, false);
    assert_eq!(code, 0, "a correct mesh must exit 0, got:\n{text}");
    assert!(text.contains("complete"), "must say so plainly:\n{text}");
    // The caveat belongs in the success message, not only in the docs: a green
    // probe means control-plane formation, not reachability.
    assert!(text.to_lowercase().contains("reachability"), "success must carry its limit:\n{text}");
}

#[test]
fn injected_defects_are_named_and_exit_nonzero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = generate(dir.path());
    let mut snapshot = complete_snapshot(&manifest);

    // Three distinct defects on two nodes.
    let sentinel = snapshot
        .nodes
        .iter_mut()
        .find(|n| n.name == "sentinel-1")
        .expect("sentinel-1");
    let first_bucket = sentinel.buckets.keys().next().expect("a bucket").clone();
    sentinel.buckets.get_mut(&first_bucket).expect("bucket").remove(0);

    let executor = snapshot
        .nodes
        .iter_mut()
        .find(|n| n.name == "executor-1")
        .expect("executor-1");
    let own = executor.rhash_hex.clone();
    executor.buckets.insert("executors".to_string(), vec![own]);

    let path = write_snapshot(dir.path(), "broken.json", &snapshot);
    let (code, text) = probe(&dir.path().join("manifest.json"), &path, false);

    assert_eq!(code, 1, "findings must exit 1:\n{text}");
    assert!(text.contains("missing"), "must classify absences:\n{text}");
    assert!(text.contains("self-present"), "must classify self-membership:\n{text}");
    assert!(text.contains("sentinel-1"), "must name the observer:\n{text}");
}

/// The most important test here: a host that never answered must never let the
/// run pass. A deployment that reads a half-observed fleet as healthy is worse
/// off than one with no probe at all.
#[test]
fn a_silent_node_fails_the_run_and_says_partial() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = generate(dir.path());
    let mut snapshot = complete_snapshot(&manifest);
    snapshot.nodes.retain(|n| n.name != "committer-1");

    let path = write_snapshot(dir.path(), "silent.json", &snapshot);
    let (code, text) = probe(&dir.path().join("manifest.json"), &path, false);

    assert_eq!(code, 1, "a silent node must not exit 0:\n{text}");
    assert!(text.contains("UNKNOWN"), "silence must not read as empty:\n{text}");
    assert!(text.contains("PARTIAL"), "the run must be marked partial:\n{text}");
}

#[test]
fn json_mode_is_machine_readable_for_ci() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = generate(dir.path());
    let mut snapshot = complete_snapshot(&manifest);
    snapshot.nodes.retain(|n| n.name != "committer-1");
    let path = write_snapshot(dir.path(), "silent.json", &snapshot);

    let (code, text) = probe(&dir.path().join("manifest.json"), &path, true);
    assert_eq!(code, 1);
    let value: serde_json::Value = serde_json::from_slice(text.as_bytes()).expect("valid json");
    assert_eq!(value["complete"], false);
    assert_eq!(value["partial"], true);
    assert_eq!(value["nodes_reported"], 3);
    assert_eq!(value["nodes_in_topology"], 4);
    let kinds: Vec<&str> = value["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|f| f["kind"].as_str().expect("kind"))
        .collect();
    assert!(kinds.contains(&"not-reported"), "got {kinds:?}");
}

/// Unusable inputs are exit 2, distinct from "the mesh is wrong" (1). A CI job
/// must not confuse "cluster broken" with "probe misconfigured".
#[test]
fn unreadable_inputs_exit_two_not_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = generate(dir.path());
    let snapshot = write_snapshot(dir.path(), "ok.json", &complete_snapshot(&manifest));
    let real_manifest = dir.path().join("manifest.json");

    // Missing required flag.
    let missing = dir.path().join("nope.json");
    let (code, text) = probe(&real_manifest, &missing, false);
    assert_eq!(code, 2, "unreadable snapshot is a usage fault, not a mesh fault:\n{text}");

    // Truncated manifest.
    let broken_manifest = dir.path().join("broken-manifest.json");
    std::fs::write(&broken_manifest, b"{ not json").expect("write");
    let (code, _) = probe(&broken_manifest, &snapshot, false);
    assert_eq!(code, 2);
}

/// A directory of fragments that contains nothing for this cluster is the most
/// common early-deployment state — the reporting path is configured, the shipper
/// has not run yet. That must read as "unobserved", never as healthy.
#[test]
fn an_empty_fragment_directory_reports_the_whole_cluster_unobserved() {
    let dir = tempfile::tempdir().expect("tempdir");
    generate(dir.path());
    let fragments = dir.path().join("fragments");
    std::fs::create_dir(&fragments).expect("mkdir");

    let out = Command::new(PROBE)
        .args([
            "--manifest",
            dir.path().join("manifest.json").to_str().unwrap(),
            "--fragments",
            fragments.to_str().unwrap(),
        ])
        .output()
        .expect("probe should run");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "no evidence cannot exit 0:\n{text}");
    assert!(text.contains("not-reported"), "every node must be named: {text}");
    assert!(text.contains("PARTIAL"), "the run must be marked partial: {text}");
    // The evidence source belongs in the output: "signed fragments" and "unsigned
    // snapshot" are different trust levels and the reader must know which they got.
    assert!(text.contains("signed fragments"), "the source must be named: {text}");
}

/// Two evidence sources would make the report ambiguous about what it judged.
#[test]
fn two_evidence_sources_are_refused_as_a_usage_fault() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = generate(dir.path());
    let snapshot = write_snapshot(dir.path(), "ok.json", &complete_snapshot(&manifest));
    let fragments = dir.path().join("fragments");
    std::fs::create_dir(&fragments).expect("mkdir");

    let out = Command::new(PROBE)
        .args([
            "--manifest",
            dir.path().join("manifest.json").to_str().unwrap(),
            "--fragments",
            fragments.to_str().unwrap(),
            "--snapshot",
            snapshot.to_str().unwrap(),
        ])
        .output()
        .expect("probe should run");
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "ambiguous inputs are a usage fault: {text}");
    assert!(text.contains("mutually exclusive"), "must say why: {text}");

    // And no evidence at all is the same class of fault.
    let out = Command::new(PROBE)
        .args(["--manifest", dir.path().join("manifest.json").to_str().unwrap()])
        .output()
        .expect("probe should run");
    assert_eq!(out.status.code(), Some(2), "missing evidence is a usage fault");
}

/// A stale directory is the quiet danger: the fleet moved on, the files did not.
#[test]
fn a_stale_fragment_directory_fails_and_says_so_through_the_cli() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = generate(dir.path());
    let fragments = dir.path().join("fragments");
    std::fs::create_dir(&fragments).expect("mkdir");
    // Write one fragment-shaped file with an ancient timestamp. It cannot verify
    // (nothing signed it), which the CLI must report rather than ignore — an
    // unverifiable fragment and a stale one both mean "not observed".
    std::fs::write(
        fragments.join("sentinel-1.fragment.json"),
        serde_json::json!({
            "version": 1,
            "written_at_unix": 1,
            "node": { "rhash_hex": manifest["nodes"][0]["rhash_hex"].as_str().unwrap(),
                       "ed25519_public_key_hex": "00" },
            "buckets": {},
            "delivery_failures": {},
            "signature": "ff",
        })
        .to_string(),
    )
    .expect("write fragment");

    let out = Command::new(PROBE)
        .args([
            "--manifest",
            dir.path().join("manifest.json").to_str().unwrap(),
            "--fragments",
            fragments.to_str().unwrap(),
            "--max-age",
            "30",
        ])
        .output()
        .expect("probe should run");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "unusable evidence cannot exit 0:\n{text}");
    assert!(text.contains("rejected evidence"), "rejections must print: {text}");
    assert!(text.contains("PARTIAL"), "the run must be marked partial: {text}");
}
