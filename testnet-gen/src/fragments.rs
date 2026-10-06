//! Reading mesh fragments written by running nodes.
//!
//! A fragment is a node's signed self-report of its own role directories
//! (`pneumatic_core::node::registry::fragment`). This module turns a directory of
//! them into the [`Snapshot`] the judge consumes, and — the part that cannot be
//! deferred — decides what to do about a fragment that is unverifiable, stale, or
//! from a node the topology does not contain.
//!
//! # Why the types come from the core crate
//!
//! `MeshFragment` is used here rather than re-declared in a local `struct`. The
//! second copy of a wire shape is where verification quietly stops meaning
//! anything: a field renamed on one side still deserializes (serde ignores
//! unknowns), so the probe would keep reporting "complete" about a format it no
//! longer understands. Sharing the type makes a format change a compile error.
//!
//! # Three ways a fragment is not evidence
//!
//! - **Bad signature.** Either it was edited in transit or the signer is not who
//!   the manifest says. Reported, and the node is treated as *not reported* —
//!   trusting a fragment we could not verify is precisely the failure this design
//!   is meant to remove.
//! - **Stale.** The timestamp is inside the signature, so age is provable and a
//!   re-dated fragment fails verification instead. A cluster judged on
//!   twenty-minute-old fragments can be told it is fine while it is on fire, so
//!   age is a finding, not a footnote.
//! - **Unknown reporter.** A node not in the manifest — an old validator still
//!   running, a mispointed directory, a node from another cluster. Its buckets
//!   are not this cluster's state.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use pneumatic_core::node::registry::fragment::MeshFragment;

use crate::probe::{Finding, ManifestIndex, NodeSnapshot, Snapshot};
use crate::topology::Mesh;

/// The keys that make a JSON document a fragment.
///
/// `signature` is required so a fragment still being written (a shipper that
/// caught a partial file, or a node killed between write and rename) is reported
/// as unreadable rather than passed over — a node that vanished from the report
/// without a word is the failure mode this whole path is meant to prevent.
fn looks_like_fragment(value: &serde_json::Value) -> bool {
    ["version", "written_at_unix", "node", "buckets", "signature"]
        .iter()
        .all(|key| value.get(*key).is_some())
}

/// How deep to walk for fragments.
///
/// A generated tree puts each node's fragment in its own subdirectory, and a
/// metrics spool typically nests by host and date, so a one-level read would
/// silently find nothing and report the whole cluster as unobserved. Two levels
/// covers both shapes; deeper than that is a directory layout this tool should
/// not be guessing at.
const MAX_FRAGMENT_DEPTH: usize = 3;

/// Collect `*.json` fragment candidates under `dir`.
///
fn collect_fragments(dir: &Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
    if depth > MAX_FRAGMENT_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        // A directory we cannot read contributes nothing. The caller reports the
        // root being unreadable; a subdirectory that appears mid-walk is not
        // worth failing the run over.
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_fragments(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json")
            && path.file_name().and_then(|n| n.to_str()) != Some("manifest.json")
        {
            out.push(path);
        }
    }
}

/// What is wrong with a fragment, and what we did about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FragmentIssue {
    /// Signature did not verify against the manifest key for this node. The
    /// fragment was ignored.
    BadSignature { node: String },
    /// Signed more than `max_age_secs` ago. The fragment was ignored.
    Stale { node: String, age_secs: u64 },
    /// Signed by a key the manifest does not list. The fragment was ignored.
    UnknownReporter { rhash_hex: String, ed25519_public_key_hex: String },
    /// Its claimed rhash is not the rhash the manifest holds for that name. The
    /// fragment was ignored.
    IdentityMismatch { node: String, claimed: String, expected: String },
    /// Written by a different fragment format than this probe reads.
    UnsupportedVersion { node: String, version: u8 },
    /// The file is not a readable fragment at all.
    Unreadable { path: String, error: String },
}

impl FragmentIssue {
    pub fn describe(&self) -> String {
        match self {
            FragmentIssue::BadSignature { node } => format!(
                "{node}'s fragment did not verify under the key manifest.json holds for it \
                 — it may have been edited or signed by another node (ignored)"),
            FragmentIssue::Stale { node, age_secs } => format!(
                "{node}'s fragment is {age_secs}s old, past the freshness bound \
                 — its directories may no longer describe reality (ignored)"),
            FragmentIssue::UnknownReporter { rhash_hex, ed25519_public_key_hex } => format!(
                "a fragment signed by rhash {rhash_hex} (ed25519 {ed25519_public_key_hex}) is \
                 not a node in this topology — stale deployment or wrong directory (ignored)"),
            FragmentIssue::IdentityMismatch { node, claimed, expected } => format!(
                "{node}'s fragment claims rhash {claimed} but manifest.json holds {expected} \
                 — keystore and manifest disagree (ignored)"),
            FragmentIssue::UnsupportedVersion { node, version } => format!(
                "{node}'s fragment is format version {version}, this probe reads version 1 \
                 — refusing to judge a partial view of an unknown shape (ignored)"),
            FragmentIssue::Unreadable { path, error } => {
                format!("{path} is not a readable fragment: {error} (ignored)")
            }
        }
    }

    /// Every issue means the same thing to the verdict: this node is not observed.
    pub fn affects_verdict(&self) -> bool {
        true
    }
}

/// The result of reading a directory of fragments.
pub struct Fragments {
    /// What survived verification, in the shape the judge takes.
    pub snapshot: Snapshot,
    /// Per-node delivery failure counts, keyed `"<node>/<bucket>/<rhash_hex>"`.
    pub delivery_failures: BTreeMap<String, u64>,
    /// Anything that was rejected, and why.
    pub issues: Vec<FragmentIssue>,
    /// Fragment-shaped documents considered, including rejected ones — so "the
    /// probe saw 3 of 12 nodes" is answerable without re-walking the directory.
    /// Unrelated JSON in the same tree is not counted; unparseable files are.
    pub files_seen: usize,
}

impl Fragments {
    /// Read every `*.json` under `dir`.
    ///
    /// `max_age_secs` bounds freshness. `now_unix` is the clock to age against —
    /// injectable because a test that sleeps to age a fragment is a test that
    /// fails on a loaded CI box.
    pub fn load(
        dir: &Path,
        cluster: &ManifestIndex,
        max_age_secs: u64,
        now_unix: u64,
    ) -> Result<Fragments, String> {
        let mut snapshot_nodes = Vec::new();
        let mut delivery_failures = BTreeMap::new();
        let mut issues = Vec::new();
        let mut files_seen = 0usize;

        let mut paths = Vec::new();
        collect_fragments(dir, 0, &mut paths);
        // Sorted for a stable report: a directory's iteration order is not.
        paths.sort();

        for path in paths {
            let display = path.display().to_string();
            let raw = match std::fs::read(&path) {
                Ok(raw) => raw,
                Err(e) => {
                    issues.push(FragmentIssue::Unreadable { path: display, error: e.to_string() });
                    continue;
                }
            };
            // Classified by shape, not by filename. A cluster directory holds other
            // JSON — `env/env.json`, `genesis.json`, keystore files — and a
            // name-based filter would either swallow those as broken fragments
            // (noise that trains an operator to ignore the rejection list) or
            // require a filename the operator cannot control once a metrics agent
            // has renamed the file on the way through. Something with the
            // fragment's keys is a fragment; anything else is not our input.
            let shape: serde_json::Value = match serde_json::from_slice(&raw) {
                Ok(value) => value,
                Err(e) => {
                    // Unparseable JSON cannot be shape-checked, so it is treated as
                    // a broken fragment rather than skipped: a shipper that caught a
                    // half-written file, or a node killed mid-write, produces exactly
                    // this. Skipping it would drop a node from the report in silence.
                    files_seen += 1;
                    issues.push(FragmentIssue::Unreadable { path: display, error: e.to_string() });
                    continue;
                }
            };
            if !looks_like_fragment(&shape) {
                // A sibling file — `genesis.json`, `env/env.json`, a keystore.
                continue;
            }
            files_seen += 1;
            let fragment: MeshFragment = match serde_json::from_value(shape) {
                Ok(fragment) => fragment,
                Err(e) => {
                    // It has the fragment's keys but not the fragment's types:
                    // a truncated or half-written write, which is exactly the
                    // thing worth reporting.
                    issues.push(FragmentIssue::Unreadable { path: display, error: e.to_string() });
                    continue;
                }
            };

            if fragment.version != 1 {
                issues.push(FragmentIssue::UnsupportedVersion {
                    // Name is untrusted until the signature checks out.
                    node: format!("rhash:{}", fragment.node.rhash_hex),
                    version: fragment.version,
                });
                continue;
            }

            // Which node does the manifest say signed this? Resolved from the
            // reporter's rhash, so an unsigned name in the file never decides
            // attribution.
            let Some(node) = cluster.node_for_rhash(&fragment.node.rhash_hex) else {
                issues.push(FragmentIssue::UnknownReporter {
                    rhash_hex: fragment.node.rhash_hex.clone(),
                    ed25519_public_key_hex: fragment.node.ed25519_public_key_hex.clone(),
                });
                continue;
            };

            let expected_key = match cluster.ed25519_for(&node) {
                Some(key) => key.clone(),
                None => {
                    issues.push(FragmentIssue::Unreadable {
                        path: display,
                        error: format!(
                            "manifest.json holds no ed25519_public_key_hex for {node}, so its \
                             fragment cannot be verified"
                        ),
                    });
                    continue;
                }
            };
            let expected_key_bytes = match hex::decode(&expected_key) {
                Ok(bytes) => bytes,
                Err(e) => {
                    issues.push(FragmentIssue::Unreadable {
                        path: display,
                        error: format!("manifest ed25519 key for {node} is not hex: {e}"),
                    });
                    continue;
                }
            };

            if !fragment.verify(&expected_key_bytes) {
                issues.push(FragmentIssue::BadSignature { node });
                continue;
            }

            // Age is checked after verification: an old fragment is only known to
            // be old by the signature that stamps it.
            let age = now_unix.saturating_sub(fragment.written_at_unix);
            if age > max_age_secs {
                issues.push(FragmentIssue::Stale { node, age_secs: age });
                continue;
            }

            // Attribution is now established, so the claimed key must agree with
            // the manifest's — a keystore swapped under a running node is exactly
            // what an operator would want to see named.
            if fragment.node.ed25519_public_key_hex != expected_key {
                issues.push(FragmentIssue::IdentityMismatch {
                    node,
                    claimed: fragment.node.ed25519_public_key_hex.clone(),
                    expected: expected_key.clone(),
                });
                continue;
            }

            snapshot_nodes.push(NodeSnapshot {
                name: node.clone(),
                rhash_hex: fragment.node.rhash_hex.clone(),
                buckets: fragment
                    .buckets
                    .iter()
                    .map(|(bucket, peers)| {
                        (bucket.clone(), peers.iter().map(|p| p.rhash_hex.clone()).collect())
                    })
                    .collect(),
            });

            for (key, count) in &fragment.delivery_failures {
                delivery_failures.insert(format!("{node}/{key}"), *count);
            }
        }

        Ok(Fragments {
            snapshot: Snapshot { collected_at: Some(now_unix.to_string()), nodes: snapshot_nodes },
            delivery_failures,
            issues,
            files_seen,
        })
    }

    /// Peers a node lists but keeps failing to deliver to.
    ///
    /// This is the closest the fragment design comes to the reachability axis: a
    /// node cannot prove a path works, but it can report that it has been failing,
    /// and "listed in the directory while undeliverable" is the exact state that
    /// has made this codebase's clusters look healthy while doing nothing.
    ///
    /// `threshold` exists because transient failures are normal during boot — the
    /// first fragments of a starting cluster will show a handful everywhere. A
    /// threshold turns a firehose into a signal; it is an operator knob, not a
    /// claim about what the number means.
    pub fn reachability(
        &self,
        mesh: &Mesh,
        cluster: &ManifestIndex,
        threshold: u64,
    ) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (key, count) in &self.delivery_failures {
            if *count < threshold {
                continue;
            }
            // Key shape: "<node>/<bucket>/<rhash_hex>".
            let mut parts = key.splitn(3, '/');
            let (Some(node), Some(_bucket), Some(rhash_hex)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            // Unknown rhashes are already reported as `UnknownPeer` by the
            // directory check; reporting them twice would double-count the same
            // fault and make the finding list lie about how many problems exist.
            let Some(peer) = cluster.node_for_rhash(rhash_hex) else { continue };
            // A peer the topology never intended this node to hold is also
            // already covered by `Missing`/`UnknownPeer`.
            let intended = mesh
                .plan
                .iter()
                .find(|p| p.node.name == *node)
                .map(|p| p.peers.iter().any(|&i| mesh.plan[i].node.name == peer))
                .unwrap_or(false);
            if !intended {
                continue;
            }
            findings.push(Finding::Unreachable {
                node: node.to_string(),
                peer,
                failures: *count,
            });
        }
        findings.sort_by_key(|f| match f {
            Finding::Unreachable { node, peer, .. } => (node.clone(), peer.clone()),
            _ => (String::new(), String::new()),
        });
        findings
    }

    /// Wall-clock seconds, for callers that do not need it injectable.
    pub fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}
