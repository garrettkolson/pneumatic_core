//! `mesh-probe` — judge a running cluster against the topology it was generated
//! from.
//!
//! ```text
//! mesh-probe --manifest <dir>/manifest.json --fragments <dir-of-fragments>
//! mesh-probe --manifest <dir>/manifest.json --snapshot <collected>.json
//! ```
//!
//! Two inputs and no network: a **manifest** (what the cluster was supposed to
//! look like, from the generator) and **evidence** (what the nodes say they hold).
//! Exit codes are the contract with CI: 0 complete, 1 findings, 2 unusable inputs.
//!
//! # Evidence: fragments, not a collector
//!
//! `--fragments` reads the signed self-reports each node writes
//! (`node::registry::fragment`). No node had to hold stake to produce them, no
//! observer needs links to the whole mesh, and anyone can re-run the judgment on
//! the same files — the only centralized thing left is the arithmetic, and that is
//! a pure function.
//!
//! `--snapshot` takes a pre-assembled snapshot, for tests and for tooling that
//! collected directories some other way. A snapshot is *unsigned*: it asserts
//! whatever it asserts. Fragments are the honest default; snapshots are the
//! escape hatch, and the report names which one it used.
//!
//! # What a clean report does *not* mean
//!
//! It means every node's directories contained what the generator intended, under
//! valid signatures, within the freshness bound. It is not a proof of
//! reachability: delivery-failure counts make *unreachability* visible
//! (`Finding::Unreachable`), but a node can only report that it has been failing
//! to use a path, never that a path works. Send traffic for the rest.
//!
//! And if any node stayed silent or had its fragment thrown away, the run is
//! **partial**. Silence is not emptiness, and a fleet half-observed must never
//! exit 0 as if it were whole.

use std::path::PathBuf;
use std::process::ExitCode;

use pneumatic_testnet_gen::cli::{flag, parsed};
use pneumatic_testnet_gen::fragments::Fragments;
use pneumatic_testnet_gen::probe::{evaluate, ManifestIndex, Snapshot};
use pneumatic_testnet_gen::topology::Mesh;

const USAGE: &str = r#"
mesh-probe — check a running cluster's role directories against its manifest

  --manifest <path>   manifest.json written by pneumatic_testnet_gen (required)
  --fragments <dir>   directory of signed per-node fragments (preferred)
  --snapshot <path>   pre-assembled snapshot, unsigned (tests and tooling)
  --max-age <secs>            ignore fragments older than this (default 30)
  --max-delivery-failures <n> flag a listed peer as unreachable past this many
                              failed deliveries (default 10)
  --json              machine-readable findings for CI
  --help              this text

Exactly one of --fragments / --snapshot is required.
Exit codes: 0 complete, 1 findings, 2 inputs unusable.
"#;

/// A fragment older than this describes a cluster that may no longer exist.
///
/// Thirty seconds: one eviction window, so a directory that has already dropped
/// its dead peers cannot be reported as current. There is no universally sane
/// value — it is a function of how fast the mesh is expected to fail — so it is a
/// flag, and the default sits at the conservative end.
const DEFAULT_MAX_AGE_SECS: u64 = 30;

/// Failed deliveries before a listed peer is called unreachable.
///
/// A booting cluster shows a few everywhere; the threshold keeps that from
/// drowning the report. It is a signal-to-noise knob, not a claim that nine
/// failures are acceptable.
const DEFAULT_MAX_DELIVERY_FAILURES: u64 = 10;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", USAGE.trim());
        return ExitCode::SUCCESS;
    }
    match run(&args, args.iter().any(|a| a == "--json")) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("[mesh-probe] error: {e}");
            ExitCode::from(2)
        }
    }
}

/// Everything the judgment needs, whichever evidence source was used.
struct Evidence {
    snapshot: Snapshot,
    /// Evidence files found on disk, including the rejected ones.
    files_seen: usize,
    /// Why each rejected fragment was unusable.
    rejected: Vec<String>,
    /// Set only for the fragment path — those files carry delivery-failure counts.
    fragments: Option<Fragments>,
    source: &'static str,
}

fn run(args: &[String], json: bool) -> Result<ExitCode, String> {
    let manifest_path = flag(args, "manifest").ok_or(format!("--manifest is required{USAGE}"))?;
    let fragments_dir = flag(args, "fragments");
    let snapshot_path = flag(args, "snapshot");
    match (&fragments_dir, &snapshot_path) {
        (Some(_), Some(_)) => {
            return Err(
                "--fragments and --snapshot are mutually exclusive: two sources of evidence \
                 would leave the report ambiguous about what it judged"
                    .to_string(),
            )
        }
        (None, None) => {
            return Err(format!(
                "one of --fragments <dir> or --snapshot <path> is required{USAGE}"
            ))
        }
        _ => {}
    }
    let max_age: u64 = parsed(args, "max-age")?.unwrap_or(DEFAULT_MAX_AGE_SECS);
    let max_failures: u64 =
        parsed(args, "max-delivery-failures")?.unwrap_or(DEFAULT_MAX_DELIVERY_FAILURES);

    let raw = std::fs::read(&manifest_path).map_err(|e| format!("read {manifest_path}: {e}"))?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("parse {manifest_path}: {e}"))?;

    // Judge against the topology as generated, not as re-derived from flags.
    let mesh = Mesh::from_manifest(&manifest)?;
    let cluster = ManifestIndex::from_manifest(&manifest)?;

    let evidence = if let Some(dir) = fragments_dir {
        let dir = PathBuf::from(dir);
        let fragments = Fragments::load(&dir, &cluster, max_age, Fragments::now())?;
        let rejected: Vec<String> = fragments.issues.iter().map(|i| i.describe()).collect();
        let files_seen = fragments.files_seen;
        let snapshot = fragments.snapshot.clone();
        Evidence { snapshot, files_seen, rejected, fragments: Some(fragments), source: "signed fragments" }
    } else {
        let snapshot = Snapshot::load(&PathBuf::from(snapshot_path.unwrap()))?;
        Evidence {
            files_seen: snapshot.nodes.len(),
            snapshot,
            rejected: Vec::new(),
            fragments: None,
            source: "unsigned snapshot",
        }
    };

    let mut report = evaluate(&mesh, &evidence.snapshot, &cluster);
    report.rejected = evidence.rejected.clone();
    if let Some(fragments) = &evidence.fragments {
        report.add_reachability(fragments.reachability(&mesh, &cluster, max_failures));
    }

    let silent = report
        .findings
        .iter()
        .filter(|f| matches!(f, pneumatic_testnet_gen::probe::Finding::NotReported { .. }))
        .count();
    // Two ways to be blind: a node that said nothing, and a node whose evidence
    // was thrown away. They add up for the operator's purposes even though the
    // causes and the fixes are nothing alike.
    let unobserved = silent + report.rejected.len();

    if json {
        let findings: Vec<serde_json::Value> = report
            .findings
            .iter()
            .map(|f| serde_json::json!({ "kind": kind_of(f), "detail": f.describe() }))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "complete": report.is_complete(),
                "partial": unobserved > 0,
                "evidence": evidence.source,
                "nodes_in_topology": mesh.plan.len(),
                "nodes_reported": evidence.snapshot.nodes.len(),
                "evidence_files_seen": evidence.files_seen,
                "max_age_secs": evidence.fragments.as_ref().map(|_| max_age),
                "expected_edges": report.expected_edges,
                "observed_edges": report.observed_edges,
                "verified": report.verified,
                "findings": findings,
                "rejected_evidence": report.rejected,
            })
        );
        return Ok(ExitCode::from(report.exit_code()));
    }

    println!(
        "mesh probe — {} nodes, {} placement, {} expected directed edges",
        mesh.plan.len(),
        mesh.placement.label(),
        report.expected_edges
    );
    println!(
        "  evidence: {} ({} file(s) read, {} node(s) usable)",
        evidence.source, evidence.files_seen, evidence.snapshot.nodes.len()
    );
    if evidence.fragments.is_some() {
        println!(
            "  freshness bound: {max_age}s · unreachable past {max_failures} failed deliveries"
        );
    }
    println!("  {} edges observed", report.observed_edges);

    if report.findings.is_empty() && report.rejected.is_empty() {
        println!("  complete: every node holds exactly the peers its topology intended");
        println!("  (control-plane formation only — reachability needs traffic)");
        return Ok(ExitCode::SUCCESS);
    }

    // Group by class: the classes have different causes, and a flat list hides
    // "one node is broken" inside "the fleet is broken".
    let mut by_kind: std::collections::BTreeMap<&'static str, Vec<String>> =
        std::collections::BTreeMap::new();
    for finding in &report.findings {
        by_kind.entry(kind_of(finding)).or_default().push(finding.describe());
    }
    println!(
        "  findings: {} across {} of {} nodes",
        report.findings.len(),
        mesh.plan.len() - report.verified.len(),
        mesh.plan.len()
    );
    for (kind, details) in &by_kind {
        println!("    {kind} ({}):", details.len());
        for detail in details.iter().take(12) {
            println!("      - {detail}");
        }
        if details.len() > 12 {
            println!("      … and {} more", details.len() - 12);
        }
    }

    // Evidence problems print apart from mesh problems: "3 nodes disagree with the
    // topology" and "3 fragments were unverifiable" need different people and
    // different fixes.
    if !report.rejected.is_empty() {
        println!("  rejected evidence ({}):", report.rejected.len());
        for detail in report.rejected.iter().take(12) {
            println!("      - {detail}");
        }
        if report.rejected.len() > 12 {
            println!("      … and {} more", report.rejected.len() - 12);
        }
    }

    if unobserved > 0 {
        // The distinction that matters most for a real deployment: an unobserved
        // host must not be read as a healthy one.
        println!(
            "  PARTIAL: {silent} node(s) reported nothing and {} fragment(s) were unusable, \
             so part of this cluster is UNKNOWN — not a clean run.",
            report.rejected.len()
        );
    }
    Ok(ExitCode::from(report.exit_code()))
}

fn kind_of(f: &pneumatic_testnet_gen::probe::Finding) -> &'static str {
    use pneumatic_testnet_gen::probe::Finding;
    match f {
        Finding::Missing { .. } => "missing",
        Finding::UnknownPeer { .. } => "unknown-peer",
        Finding::WrongBucket { .. } => "wrong-bucket",
        Finding::SelfPresent { .. } => "self-present",
        Finding::NotReported { .. } => "not-reported",
        Finding::Unreachable { .. } => "unreachable",
        Finding::IdentityMismatch { .. } => "identity-mismatch",
    }
}
