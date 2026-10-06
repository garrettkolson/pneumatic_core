//! End-to-end tests of the **binary**, not the library.
//!
//! Flag parsing lives in `main.rs`, outside the library, so nothing else can
//! reach it. These invoke the built binary the way an operator does and read
//! back what it wrote — the only way to catch a flag that is accepted but
//! ignored, or a count that is silently truncated.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_pneumatic_testnet_gen")
}

/// The repo's env template. The CLI's default is CWD-relative, and a test runs
/// from the package directory, so it must be passed explicitly.
fn env_template() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../deploy/config/env/env.json")
        .to_string_lossy()
        .to_string()
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl Run {
    fn fail(&self, context: &str) {
        assert!(
            self.status.success(),
            "{context}: exit {:?}\nstdout:\n{}\nstderr:\n{}",
            self.status,
            self.stdout,
            self.stderr
        );
    }
}

fn run(args: &[&str]) -> Run {
    let output = Command::new(binary())
        .arg("--env-template")
        .arg(env_template())
        .args(args)
        .output()
        .expect("the generator binary should run");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        status: output.status,
    }
}

fn manifest(out: &Path) -> Value {
    let raw = std::fs::read(out.join("manifest.json")).expect("manifest.json");
    serde_json::from_slice(&raw).expect("manifest should parse")
}

/// `--validators n` is a *count of validators*. It used to divide by four and
/// throw away the remainder, so `--validators 10` built 8 nodes — a shortfall
/// invisible unless you counted the output.
#[test]
fn validators_flag_produces_exactly_that_many_nodes() {
    let dir = tempfile::tempdir().expect("tempdir");
    run(&[
        "--out",
        dir.path().to_str().unwrap(),
        "--validators",
        "10",
    ])
    .fail("--validators 10 must succeed");

    let manifest = manifest(dir.path());
    let nodes = manifest["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 10, "--validators 10 must make 10 nodes");

    let mut per_role: std::collections::BTreeMap<String, usize> = Default::default();
    for node in nodes {
        *per_role
            .entry(node["role"].as_str().unwrap().to_lowercase())
            .or_default() += 1;
    }
    // The remainder goes to the first roles in order: 10 = 3+3+2+2.
    assert_eq!(
        per_role,
        std::collections::BTreeMap::from([
            ("sentinel".to_string(), 3),
            ("executor".to_string(), 3),
            ("finalizer".to_string(), 2),
            ("committer".to_string(), 2),
        ]),
        "uneven split must be deterministic and role-ordered"
    );
}

/// A provisioner's address list that does not match the cluster must stop the
/// run. Padding with loopback would produce a fleet where some nodes dial
/// themselves, which boots fine and connects to nobody.
#[test]
fn a_short_address_list_stops_the_run_and_names_both_counts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let addresses = dir.path().join("addresses.txt");
    std::fs::write(&addresses, "10.0.0.1\n10.0.0.2\n").expect("write addresses");

    let out = dir.path().join("net");
    let run = run(&[
        "--out",
        out.to_str().unwrap(),
        "--validators",
        "4",
        "--addresses-file",
        addresses.to_str().unwrap(),
    ]);
    assert!(
        !run.status.success(),
        "2 addresses for 4 nodes must not generate a cluster"
    );
    let message = format!("{}{}", run.stderr, run.stdout);
    assert!(
        message.contains('2') && message.contains('4'),
        "the error must name the count supplied and the count required, got: {message}"
    );
}

/// The address file is meant to be a provisioner's output, so comments and
/// blank lines are normal and must not become phantom nodes.
#[test]
fn an_address_file_ignores_comments_and_blank_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let addresses = dir.path().join("addresses.txt");
    std::fs::write(
        &addresses,
        "# private ips from terraform\n10.0.1.11\n\n10.0.1.12\n# tail comment\n10.0.1.13\n10.0.1.14\n",
    )
    .expect("write addresses");

    let out = dir.path().join("net");
    run(&[
        "--out",
        out.to_str().unwrap(),
        "--validators",
        "4",
        "--addresses-file",
        addresses.to_str().unwrap(),
    ])
    .fail("a commented address file must work");

    let manifest = manifest(&out);
    assert_eq!(manifest["placement"].as_str(), Some("per-host"));
    let nodes = manifest["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 4);

    // One shared port range is the firewall consequence of per-host placement.
    let bases: Vec<&Value> = nodes.iter().map(|n| &n["rns_port"]).collect();
    assert!(
        bases.windows(2).all(|w| w[0] == w[1]),
        "per-host placement must reuse one base port, got {bases:?}"
    );
    // And each peer entry carries the peer's address, read straight from disk.
    for node in nodes {
        let path = Path::new(node["dir"].as_str().unwrap()).join("config.json");
        let config: Value = serde_json::from_slice(&std::fs::read(&path).expect("config"))
            .expect("parse");
        for peer in config["bootstrap_peers"].as_array().unwrap() {
            let dialed = peer["ip"].as_str().unwrap();
            assert!(
                dialed.starts_with("10.0.1."),
                "{dialed} is not one of the supplied addresses"
            );
        }
    }
}

/// `--addresses` and `--addresses-file` contradict each other; refusing beats
/// picking one and leaving the operator to guess which.
#[test]
fn mutually_exclusive_address_flags_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let addresses = dir.path().join("addresses.txt");
    std::fs::write(&addresses, "10.0.0.1\n10.0.0.2\n10.0.0.3\n10.0.0.4\n").unwrap();

    let run = run(&[
        "--out",
        dir.path().join("net").to_str().unwrap(),
        "--validators",
        "4",
        "--addresses",
        "10.9.9.1,10.9.9.2,10.9.9.3,10.9.9.4",
        "--addresses-file",
        addresses.to_str().unwrap(),
    ]);
    assert!(
        !run.status.success(),
        "supplying both address flags must be refused"
    );
    assert!(
        format!("{}{}", run.stderr, run.stdout).contains("mutually exclusive"),
        "the error must say why"
    );
}
