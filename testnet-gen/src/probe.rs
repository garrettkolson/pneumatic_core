//! Mesh verification: what the topology *promises* versus what nodes *report*.
//!
//! # Why this exists as its own thing
//!
//! Every defect found in this area so far had the same signature: a node that
//! boots, announces itself, logs nothing worse than "no live route", and peers
//! with nobody. A log-scraping check cannot see that, because the successful
//! registration path emits nothing — only rejections do (`registration.rs:274`,
//! `:504`, `:550`). Healthy and broken are indistinguishable in the logs.
//!
//! So the answer has to be asked of the nodes themselves, through the directory
//! protocol, and *judged* against what the generator intended. This module is
//! the judgment. It has no I/O: it takes the mesh the generator built and the
//! snapshots the nodes reported, and says precisely who is missing from whose
//! directory. The collector that fills snapshots is separate, which keeps the
//! part with the most ways to be wrong testable without a network.
//!
//! # The three states this refuses to conflate
//!
//! A directory can be **complete**, **incomplete**, or **silent**, and only the
//! first two mean what they look like. A node that never answered is not a node
//! with an empty mesh; reporting them the same way is how a dead host gets read
//! as a healthy one. Silence gets its own finding (`NotReported`).
//!
//! Reachability is a *third* axis, only half of which is visible. A peer can sit
//! in a directory with no live route to it — "peers discovered" is not "peers
//! reachable", which is the whole reason leaves need direct links. Fragments carry
//! each node's own failed-delivery counts, so *unreachability* is observable
//! (`Finding::Unreachable`); reachability itself never is, because a node cannot
//! report a path it has not had to use. A complete report means the control plane
//! formed and nothing is known to be failing. Send traffic for the rest.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::topology::{Role, Mesh};

/// What one node reported about its own role directories.
///
/// The wire shape is deliberately dumb: role name to a list of peer rhashes. A
/// collector can fill it from directory responses without knowing anything about
/// what is expected, which is the point — the reporter should not be able to
/// influence the verdict.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeSnapshot {
    /// Node name as it appears in `manifest.json`.
    pub name: String,
    /// The reporting node's own rhash, hex. Cross-checked against the manifest:
    /// a snapshot attributed to the wrong node would otherwise pass unnoticed.
    pub rhash_hex: String,
    /// Role name → rhashes this node holds in that bucket. Missing keys are
    /// treated as empty buckets.
    #[serde(default)]
    pub buckets: BTreeMap<String, Vec<String>>,
}

/// Everything a collector gathered for one cluster, in one run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// Free-form marker for the report header (host, timestamp, run id).
    #[serde(default)]
    pub collected_at: Option<String>,
    pub nodes: Vec<NodeSnapshot>,
}

impl Snapshot {
    /// Read a snapshot from disk. The format is this module's contract with
    /// whatever collector produces it.
    pub fn load(path: &std::path::Path) -> Result<Snapshot, String> {
        let raw = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_json::from_slice(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
    }
}

/// One discrepancy. Each variant is a different failure with a different cause,
/// so they are not collapsed into "expected != actual".
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Finding {
    /// The topology says `observer` should hold `missing_node` in its
    /// `role` bucket, and it does not.
    Missing {
        observer: String,
        role: String,
        missing_node: String,
    },
    /// `observer` holds an rhash that is not a cluster member at all — usually a
    /// stale route from a previous run, or a mis-pointed data service.
    UnknownPeer {
        observer: String,
        role: String,
        rhash_hex: String,
    },
    /// `observer` holds `node` in the wrong bucket. This is the shape of the
    /// RegisterAck bucket-placement defect: the peer is known, filed under
    /// whatever the *requester* declared instead of what it is.
    WrongBucket {
        observer: String,
        node: String,
        found_in: String,
        should_be: Vec<String>,
    },
    /// `observer` lists *itself*. A node is not its own peer; this was the exact
    /// symptom of installing an ack's responder under the acking node's role.
    SelfPresent { observer: String },
    /// No snapshot for a node the topology includes. **Not** the same as an
    /// empty directory: this node never answered, so nothing about its mesh is
    /// known.
    NotReported { node: String },
    /// This node repeatedly failed to deliver to a peer it holds in a bucket —
    /// *listed and unreachable*, the state a directory dump alone cannot see.
    ///
    /// The count is the node's own, from its fragment: monotonic per process, so
    /// this reads as "we have been failing to reach it", not as a proof about the
    /// path. Fragments still cannot prove reachability — only its absence is
    /// observable, which is the honest half.
    Unreachable { node: String, peer: String, failures: u64 },
    /// The snapshot's rhash disagrees with the manifest's for that name. Two
    /// different clusters, or a copied directory.
    IdentityMismatch { node: String, expected: String, found: String },
}

impl Finding {
    /// Whether this finding is a statement about `node`'s own directories.
    ///
    /// One rule instead of a match per caller: `evaluate` uses it to decide which
    /// nodes count as verified, and [`MeshReport::add_reachability`] uses it to
    /// undo that verdict when new evidence arrives. Two copies of "whose fault is
    /// this" is how a node ends up listed as verified while carrying findings.
    ///
    /// `NotReported` and `IdentityMismatch` are excluded because they describe the
    /// *absence* or *identity* of a node rather than the contents of its buckets;
    /// a silent node never reaches the verified list in the first place.
    pub fn concerns(&self, node: &str) -> bool {
        match self {
            Finding::Missing { observer, .. }
            | Finding::UnknownPeer { observer, .. }
            | Finding::WrongBucket { observer, .. }
            | Finding::SelfPresent { observer } => observer == node,
            Finding::Unreachable { node: owner, .. } => owner == node,
            Finding::NotReported { .. } | Finding::IdentityMismatch { .. } => false,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Finding::Missing { observer, role, missing_node } =>
                format!("{observer} is missing {missing_node} from its {role} bucket"),
            Finding::UnknownPeer { observer, role, rhash_hex } =>
                format!("{observer} holds unknown rhash {rhash_hex} in its {role} bucket (not a member of this cluster)"),
            Finding::WrongBucket { observer, node, found_in, should_be } =>
                format!("{observer} holds {node} in its {found_in} bucket, but its declared roles are {} — expected in {}",
                    should_be.join("/"), should_be.join("/")),
            Finding::SelfPresent { observer } =>
                format!("{observer} lists itself as a peer (a node is not its own peer)"),
            Finding::NotReported { node } =>
                format!("{node} reported nothing — its mesh state is UNKNOWN, not empty"),
            Finding::Unreachable { node, peer, failures } =>
                format!("{node} recorded {failures} failed deliveries to {peer}, which it lists as a peer — known-and-unreachable is not the same as peered"),
            Finding::IdentityMismatch { node, expected, found } =>
                format!("{node}'s snapshot rhash {found} does not match the manifest's {expected}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MeshReport {
    pub findings: Vec<Finding>,
    /// Nodes whose snapshot arrived and matched expectations exactly.
    pub verified: Vec<String>,
    /// Total expected directed edges in the topology. Quoted so "12 findings out
    /// of 12 edges" and "12 findings out of 1560 edges" do not read the same.
    pub expected_edges: usize,
    /// Edges actually observed, counted across reported nodes only.
    pub observed_edges: usize,
    /// Fragments that were present but unusable — unverifiable, stale, from an
    /// unknown node. A separate axis from `findings` on purpose: those are failed
    /// claims about the mesh, these are evidence that never arrived, and an
    /// operator needs to know which kind of blindness they are in.
    pub rejected: Vec<String>,
}

impl MeshReport {
    /// Fold reachability findings in without letting `verified` go stale.
    ///
    /// `verified` is computed inside [`evaluate`], which knows nothing about
    /// delivery failures. Adding these findings without revisiting the list would
    /// print a node as both verified and unable to reach its peers — a report that
    /// contradicts itself is worse than one that under-reports, because the reader
    /// has to guess which half to believe.
    pub fn add_reachability(&mut self, extra: Vec<Finding>) {
        self.findings.extend(extra);
        self.findings.sort();
        let flagged: std::collections::BTreeSet<String> = self
            .verified
            .iter()
            .filter(|name| self.findings.iter().any(|f| f.concerns(name)))
            .cloned()
            .collect();
        self.verified.retain(|name| !flagged.contains(name));
    }

    /// Complete means every expectation matched **and** nothing had to be thrown
    /// away to get there. A report built from 4 of 12 fragments is not a clean
    /// report of a cluster — it is a clean report of a quarter of one.
    pub fn is_complete(&self) -> bool {
        self.findings.is_empty() && self.rejected.is_empty()
    }

    /// Exit code for CI: 0 complete, 1 findings. (`2` is reserved for a caller
    /// that could not evaluate at all — unreadable inputs — which this type
    /// never sees.) `u8` because `ExitCode::from` takes `u8`, and casting at
    /// every call site is where a negative value would hide.
    pub fn exit_code(&self) -> u8 {
        u8::from(!self.is_complete())
    }
}

/// Name↔rhash index for a cluster, read from `manifest.json`.
///
/// Two explicit maps rather than one bidirectional one. A single map holding
/// both `name → rhash` and `rhash → name` makes a node's *name* look like a
/// member it holds, and makes any future `contains_key(rhash)` query silently
/// wrong; the day someone passes the wrong direction, the verdict changes
/// without a type error. The two maps make "which direction" part of the call.
#[derive(Clone, Debug, Default)]
pub struct ManifestIndex {
    by_name: BTreeMap<String, String>,
    by_rhash: BTreeMap<String, String>,
    /// `name → ed25519 verifying key (hex)`: the key a fragment must verify
    /// under. Optional in the manifest because a hand-made snapshot needs no
    /// signatures; absent keys make fragment verification impossible, not optional.
    keys_by_name: BTreeMap<String, String>,
}

impl ManifestIndex {
    /// Build the index from a parsed `manifest.json`.
    ///
    /// Duplicate rhashes are refused: two nodes claiming one identity would make
    /// every membership test ambiguous, and the report would blame the wrong one.
    pub fn from_manifest(manifest: &serde_json::Value) -> Result<ManifestIndex, String> {
        let nodes = manifest["nodes"].as_array().ok_or("manifest.nodes missing")?;
        let mut index = ManifestIndex::default();
        for node in nodes {
            let name = node["name"].as_str().ok_or("manifest node missing name")?.to_string();
            let rhash = node["rhash_hex"].as_str().ok_or("manifest node missing rhash_hex")?;
            if let Some(previous) = index.by_rhash.insert(rhash.to_string(), name.clone()) {
                return Err(format!("rhash {rhash} is claimed by both {previous} and {name}"));
            }
            if let Some(key) = node["ed25519_public_key_hex"].as_str() {
                index.keys_by_name.insert(name.clone(), key.to_string());
            }
            index.by_name.insert(name, rhash.to_string());
        }
        Ok(index)
    }

    pub fn rhash_for(&self, name: &str) -> Option<&String> {
        self.by_name.get(name)
    }

    pub fn name_for(&self, rhash: &str) -> Option<&String> {
        self.by_rhash.get(rhash)
    }

    pub fn is_member(&self, rhash: &str) -> bool {
        self.by_rhash.contains_key(rhash)
    }

    /// Which node reports this rhash. Attribution for a fragment is resolved from
    /// the signed rhash, never from a name in the file: an unsigned field must not
    /// decide who a report is attributed to.
    pub fn node_for_rhash(&self, rhash_hex: &str) -> Option<String> {
        self.by_rhash.get(rhash_hex).cloned()
    }

    /// The verifying key the manifest holds for `name`.
    pub fn ed25519_for(&self, name: &str) -> Option<&String> {
        self.keys_by_name.get(name).filter(|key| !key.is_empty())
    }
}

/// Judge `snapshot` against `mesh`.
///
/// `cluster` is the manifest's name↔rhash index, so an rhash in a directory can
/// be resolved to the node it names — without it the report can only say
/// "something is missing", which is not actionable.
pub fn evaluate(
    mesh: &Mesh,
    snapshot: &Snapshot,
    cluster: &ManifestIndex,
) -> MeshReport {
    let mut findings = Vec::new();
    let mut verified = Vec::new();
    // The topology's own total, taken from the mesh rather than from whatever
    // evidence arrived. Counting only reported nodes would make an unobserved
    // cluster advertise *fewer* edges to check — a fleet with no fragments on disk
    // would print "0 expected directed edges", which reads as "nothing to verify"
    // when the truth is "nothing has been verified".
    let expected_edges: usize = mesh.plan.iter().map(|p| p.peers.len()).sum();
    let mut observed_edges = 0usize;

    let reported: BTreeMap<String, &NodeSnapshot> = snapshot
        .nodes
        .iter()
        .map(|n| (n.name.clone(), n))
        .collect();

    // A node in the topology that never reported: unknown, not empty.
    for plan in &mesh.plan {
        if !reported.contains_key(&plan.node.name) {
            findings.push(Finding::NotReported { node: plan.node.name.clone() });
        }
    }

    for snapshot_node in &snapshot.nodes {
        let plan = match mesh.plan.iter().find(|p| p.node.name == snapshot_node.name) {
            Some(plan) => plan,
            // A snapshot for a node this topology never generated. Flag it as
            // unknown rather than silently ignoring it: two clusters' outputs
            // merged is exactly the mistake a report must not hide.
            None => {
                findings.push(Finding::UnknownPeer {
                    observer: snapshot_node.name.clone(),
                    role: "(not in topology)".to_string(),
                    rhash_hex: snapshot_node.rhash_hex.clone(),
                });
                continue;
            }
        };

        let expected_rhash = cluster
            .rhash_for(&plan.node.name)
            .cloned()
            .unwrap_or_else(|| String::from("(no rhash in manifest)"));
        if !expected_rhash.is_empty() && snapshot_node.rhash_hex != expected_rhash {
            findings.push(Finding::IdentityMismatch {
                node: plan.node.name.clone(),
                expected: expected_rhash,
                found: snapshot_node.rhash_hex.clone(),
            });
        }

        // Bucket → the set of members that bucket *may* legitimately hold.
        // Derived from each peer's declared-role set (the roles its stake
        // qualifies for), so a composite that runs four roles is expected in
        // four buckets per observer, not one. Role-graph sparsity is expected
        // here rather than reported as missing.
        let mut expected: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for &peer_index in &plan.peers {
            let peer = &mesh.plan[peer_index];
            for role in &peer.node.roles {
                expected
                    .entry(role.plural().to_string())
                    .or_default()
                    .push(peer.node.name.clone());
            }
        }

        // Index everything the node reported, by rhash, with the buckets found in.
        let mut found_by_rhash: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (role, rhashes) in &snapshot_node.buckets {
            for rhash in rhashes {
                found_by_rhash
                    .entry(rhash.clone())
                    .or_default()
                    .push(role.clone());
            }
        }

        // A mesh link is observed when the reporter holds that peer in at least
        // one bucket, no matter how many of the peer's declared role buckets
        // carry it. Counting links — not bucket memberships — keeps
        // `observed_edges` directly comparable to `expected_edges` under the
        // multi-role model, where one honest peer legitimately fills four.
        for &peer_index in &plan.peers {
            let peer_name = &mesh.plan[peer_index].node.name;
            if let Some(rhash) = cluster.rhash_for(peer_name) {
                if found_by_rhash.contains_key(rhash) {
                    observed_edges += 1;
                }
            }
        }

        // Self-presence, checked before anything else: it is the clearest signal
        // of the ack defect and it poisons the other comparisons.
        if found_by_rhash.contains_key(&snapshot_node.rhash_hex) {
            findings.push(Finding::SelfPresent { observer: plan.node.name.clone() });
        }

        // Missing: expected member whose rhash is absent from the right bucket.
        for (role, members) in &expected {
            let held: Vec<&String> = snapshot_node
                .buckets
                .get(role)
                .map(|held| held.iter().collect())
                .unwrap_or_default();
            for member in members {
                let rhash = match cluster.rhash_for(member) {
                    Some(r) => r,
                    None => continue, // manifest lacks it; not this node's fault
                };
                if !held.contains(&rhash) {
                    findings.push(Finding::Missing {
                        observer: plan.node.name.clone(),
                        role: role.clone(),
                        missing_node: member.clone(),
                    });
                }
            }
        }

        // Wrong bucket and unknown peers.
        for (rhash, buckets) in &found_by_rhash {
            if rhash == &snapshot_node.rhash_hex {
                continue; // already reported as SelfPresent
            }
            let member = match cluster.name_for(rhash) {
                Some(name) => name.clone(),
                None => {
                    for role in buckets {
                        findings.push(Finding::UnknownPeer {
                            observer: plan.node.name.clone(),
                            role: role.clone(),
                            rhash_hex: rhash.clone(),
                        });
                    }
                    continue;
                }
            };
            // The buckets this member legitimately belongs in: every role it
            // declares (its stake-qualifying set), intersected with the fact
            // that it IS a peer of this observer. A composite declaring four
            // roles is expected in four buckets, so extra buckets are correct,
            // not wrong-bucket.
            let should_be: Vec<String> = plan
                .peers
                .iter()
                .map(|&i| mesh.plan[i].clone())
                .filter(|p| p.node.name == member)
                .flat_map(|p| p.node.roles.iter().map(|r| r.plural().to_string()).collect::<Vec<_>>())
                .collect();
            if should_be.is_empty() {
                // Known cluster member, but not a peer of this node at all.
                for role in buckets {
                    findings.push(Finding::UnknownPeer {
                        observer: plan.node.name.clone(),
                        role: role.clone(),
                        rhash_hex: rhash.clone(),
                    });
                }
                continue;
            }
            for role in buckets {
                if !should_be.contains(role) {
                    findings.push(Finding::WrongBucket {
                        observer: plan.node.name.clone(),
                        node: member.clone(),
                        found_in: role.clone(),
                        should_be: should_be.clone(),
                    });
                }
            }
        }

        let own_findings = findings
            .iter()
            .any(|f| f.concerns(&plan.node.name));
        if !own_findings {
            verified.push(plan.node.name.clone());
        }
    }

    findings.sort();
    MeshReport { findings, verified, expected_edges, observed_edges, rejected: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{Placement, TopologyMode};

    /// A stand-in rhash for each node. Real ones are 32 bytes hex; the probe only
    /// compares them, so any stable distinct token works.
    fn rhash_for(index: usize) -> String {
        format!("{index:0>64x}")
    }

    /// Build a mesh plus the manifest-shaped rhash table it would be judged with.
    fn fixture(mode: TopologyMode) -> (Mesh, Snapshot, ManifestIndex) {
        fixture_counts(mode, 2, 1, 1, 1)
    }

    fn fixture_counts(
        mode: TopologyMode,
        sentinels: usize,
        executors: usize,
        finalizers: usize,
        committers: usize,
    ) -> (Mesh, Snapshot, ManifestIndex) {
        let mesh = Mesh::build(
            [
                (Role::Sentinel, sentinels),
                (Role::Executor, executors),
                (Role::Finalizer, finalizers),
                (Role::Committer, committers),
            ],
            20_000,
            mode,
            Placement::SingleHost,
        )
        .expect("mesh builds");

        // Synthesize the manifest the cluster would have shipped with.
        let manifest = serde_json::json!({
            "nodes": mesh.plan.iter().map(|p| serde_json::json!({
                "name": p.node.name,
                "rhash_hex": rhash_for(p.node.index),
            })).collect::<Vec<_>>(),
        });
        let index = ManifestIndex::from_manifest(&manifest).expect("fixture manifest");

        let snapshot = Snapshot { collected_at: None, nodes: Vec::new() };
        (mesh, snapshot, index)
    }

    /// A complete snapshot: every node reports exactly its topology peers, each
    /// under the right role bucket.
    fn complete_snapshot(mesh: &Mesh) -> Snapshot {
        let nodes = mesh
            .plan
            .iter()
            .map(|plan| {
                let mut buckets: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for &peer in &plan.peers {
                    let p = &mesh.plan[peer];
                    buckets
                        .entry(p.node.role.plural().to_string())
                        .or_default()
                        .push(rhash_for(p.node.index));
                }
                NodeSnapshot {
                    name: plan.node.name.clone(),
                    rhash_hex: rhash_for(plan.node.index),
                    buckets,
                }
            })
            .collect();
        Snapshot { collected_at: Some("fixture".to_string()), nodes }
    }

    #[test]
    fn a_complete_mesh_verifies_with_no_findings() {
        let (mesh, _, names) = fixture(TopologyMode::FullMesh);
        let snapshot = complete_snapshot(&mesh);
        let report = evaluate(&mesh, &snapshot, &names);
        assert!(
            report.is_complete(),
            "a correct mesh must verify clean, got: {:?}",
            report.findings.iter().map(|f| f.describe()).collect::<Vec<_>>()
        );
        assert_eq!(report.verified.len(), mesh.plan.len());
        // The fixture is 5 nodes (2 sentinels + one of each other role), so a
        // full mesh is 4 directed edges per node.
        assert_eq!(report.expected_edges, 20);
        assert_eq!(report.observed_edges, 20);
        assert_eq!(report.exit_code(), 0);
    }

    /// The whole point of the tool: name the edge, not just "the mesh is wrong".
    /// The denominator belongs to the topology, not to the evidence.
    ///
    /// An empty snapshot used to advertise `0 expected directed edges`, which is
    /// how a monitoring dashboard ends up reading "nothing to check" about a fleet
    /// whose reporting has not started.
    #[test]
    fn expected_edges_describe_the_topology_even_when_nobody_reports() {
        let (mesh, _, names) = fixture(TopologyMode::FullMesh);
        let report = evaluate(&mesh, &Snapshot { collected_at: None, nodes: Vec::new() }, &names);
        assert_eq!(report.expected_edges, 20, "5 nodes, full mesh");
        assert_eq!(report.observed_edges, 0, "nothing was seen");
        assert!(!report.is_complete(), "no evidence is never a clean run");
        assert_eq!(report.findings.len(), mesh.plan.len());
    }

    #[test]
    fn a_dropped_entry_names_the_exact_edge_in_both_directions() {
        let (mesh, _, names) = fixture(TopologyMode::FullMesh);
        let mut snapshot = complete_snapshot(&mesh);

        // finalizer-1 drops sentinel-2 from its Sentinels bucket.
        let finalizer = snapshot
            .nodes
            .iter_mut()
            .find(|n| n.name == "finalizer-1")
            .expect("finalizer-1 present");
        let sentinel2 = rhash_for(
            mesh.plan
                .iter()
                .find(|p| p.node.name == "sentinel-2")
                .expect("sentinel-2")
                .node
                .index,
        );
        let bucket = finalizer.buckets.get_mut("sentinels").expect("sentinels bucket");
        bucket.retain(|r| r != &sentinel2);

        let report = evaluate(&mesh, &snapshot, &names);
        assert!(!report.is_complete());
        assert!(
            report.findings.iter().any(|f| matches!(
                f,
                Finding::Missing { observer, role, missing_node }
                    if observer == "finalizer-1"
                        && role == "sentinels"
                        && missing_node == "sentinel-2"
            )),
            "must name the missing edge, got: {:?}",
            report.findings.iter().map(|f| f.describe()).collect::<Vec<_>>()
        );
        assert!(!report.is_complete());
        assert_eq!(report.exit_code(), 1);
        // And finalizer-1 must not be counted verified.
        assert!(!report.verified.contains(&"finalizer-1".to_string()));
    }

    /// A node listing itself was the exact symptom of the RegisterAck defect:
    /// the responder installed under its own declared role.
    #[test]
    fn a_node_listing_itself_is_called_out() {
        let (mesh, _, names) = fixture(TopologyMode::FullMesh);
        let mut snapshot = complete_snapshot(&mesh);
        let executor = snapshot
            .nodes
            .iter_mut()
            .find(|n| n.name == "executor-1")
            .expect("executor-1");
        let own = executor.rhash_hex.clone();
        executor
            .buckets
            .entry("executors".to_string())
            .or_default()
            .push(own);

        let report = evaluate(&mesh, &snapshot, &names);
        assert!(
            report.findings.iter().any(|f| matches!(
                f,
                Finding::SelfPresent { observer } if observer == "executor-1"
            )),
            "self-membership must be reported, got: {:?}",
            report.findings.iter().map(|f| f.describe()).collect::<Vec<_>>()
        );
    }

    /// A directory entry that is not part of this cluster — a stale route from a
    /// previous run, or a data service pointing somewhere else.
    #[test]
    fn an_unknown_rhash_is_reported_as_unknown() {
        let (mesh, _, names) = fixture(TopologyMode::FullMesh);
        let mut snapshot = complete_snapshot(&mesh);
        snapshot
            .nodes
            .iter_mut()
            .find(|n| n.name == "committer-1")
            .expect("committer-1")
            .buckets
            .entry("sentinels".to_string())
            .or_default()
            .push("ffffffff".repeat(8));

        let report = evaluate(&mesh, &snapshot, &names);
        assert!(
            report.findings.iter().any(|f| matches!(
                f,
                Finding::UnknownPeer { observer, .. } if observer == "committer-1"
            )),
            "got: {:?}",
            report.findings.iter().map(|f| f.describe()).collect::<Vec<_>>()
        );
    }

    /// Present-but-misfiled: known peer, wrong bucket. This is the shape the ack
    /// defect produced *before* it was fixed, when a responder landed under the
    /// requester's role.
    #[test]
    fn a_peer_in_the_wrong_bucket_is_reported_as_misfiled() {
        let (mesh, _, names) = fixture(TopologyMode::FullMesh);
        let mut snapshot = complete_snapshot(&mesh);
        let sentinel = snapshot
            .nodes
            .iter_mut()
            .find(|n| n.name == "sentinel-1")
            .expect("sentinel-1");
        // Move finalizer-1 out of "finalizers" and into "committers".
        let finalizer_rhash = rhash_for(
            mesh.plan.iter().find(|p| p.node.name == "finalizer-1").unwrap().node.index,
        );
        if let Some(bucket) = sentinel.buckets.get_mut("finalizers") {
            bucket.retain(|r| *r != finalizer_rhash);
        }
        sentinel
            .buckets
            .entry("committers".to_string())
            .or_default()
            .push(finalizer_rhash.clone());
        // Drop it from committers' expected slot too, so the only complaint left
        // is the misfiling rather than a plain absence of the committer entry.

        let report = evaluate(&mesh, &snapshot, &names);
        assert!(
            report.findings.iter().any(|f| matches!(
                f,
                Finding::WrongBucket { observer, node, found_in, .. }
                    if observer == "sentinel-1" && node == "finalizer-1" && found_in == "committers"
            )),
            "misfiling must be distinguished from absence, got: {:?}",
            report.findings.iter().map(|f| f.describe()).collect::<Vec<_>>()
        );
    }

    /// **Silence is not emptiness.** A host that never answered must not read as
    /// a node with an empty mesh, and must not let the run pass.
    #[test]
    fn a_silent_node_is_unknown_not_empty() {
        let (mesh, _, names) = fixture(TopologyMode::FullMesh);
        let mut snapshot = complete_snapshot(&mesh);
        snapshot.nodes.retain(|n| n.name != "committer-1");

        let report = evaluate(&mesh, &snapshot, &names);
        assert!(
            report.findings.iter().any(|f| matches!(
                f,
                Finding::NotReported { node } if node == "committer-1"
            )),
            "a missing snapshot must be its own finding class"
        );
        assert!(!report.is_complete(), "a silent node must never let the run pass");
        // And the wording must say UNKNOWN, not "0 peers".
        let described = report
            .findings
            .iter()
            .map(|f| f.describe())
            .find(|d| d.contains("committer-1"))
            .expect("committer-1 finding");
        assert!(described.contains("UNKNOWN"), "wording must be explicit: {described}");
    }

    /// Role-graph clusters legitimately omit intra-executor links. They must not
    /// be reported as missing — the expectation comes from the mesh, not from a
    /// full-mesh assumption baked into the probe.
    /// With two executors, the role graph genuinely drops the executor↔executor
    /// links (`fact-fanout-graph-density`). The probe must treat that as correct
    /// rather than as two missing edges — the expectation comes from the mesh.
    #[test]
    fn role_graph_sparsity_is_expected_not_missing() {
        let (mesh, _, names) = fixture_counts(TopologyMode::RoleGraph, 1, 2, 1, 1);
        let snapshot = complete_snapshot(&mesh);
        let report = evaluate(&mesh, &snapshot, &names);
        assert!(
            report.is_complete(),
            "a role-graph cluster must verify against role-graph expectations, got: {:?}",
            report.findings.iter().map(|f| f.describe()).collect::<Vec<_>>()
        );

        // The sparsity must be real in this fixture, or the test above proves
        // nothing: no executor may hold another executor.
        let executors: Vec<&String> = mesh
            .plan
            .iter()
            .filter(|p| p.node.role == Role::Executor)
            .map(|p| &p.node.name)
            .collect();
        assert_eq!(executors.len(), 2, "fixture must have two executors to show sparsity");
        for plan in &mesh.plan {
            if plan.node.role == Role::Executor {
                for peer in &plan.peers {
                    assert_ne!(
                        mesh.plan[*peer].node.role,
                        Role::Executor,
                        "role graph must drop intra-executor links"
                    );
                }
            }
        }

        // And the same node counts under full mesh have strictly more edges, so
        // "complete" cannot mean "whatever the snapshot happens to contain".
        let (full, _, _) = fixture_counts(TopologyMode::FullMesh, 1, 2, 1, 1);
        assert!(
            mesh.plan.iter().map(|p| p.peers.len()).sum::<usize>()
                < full.plan.iter().map(|p| p.peers.len()).sum::<usize>(),
            "role graph must be sparser than full mesh for these counts"
        );
    }

    #[test]
    fn identity_mismatch_between_snapshot_and_manifest_is_caught() {
        let (mesh, _, names) = fixture(TopologyMode::FullMesh);
        let mut snapshot = complete_snapshot(&mesh);
        snapshot
            .nodes
            .iter_mut()
            .find(|n| n.name == "sentinel-1")
            .expect("sentinel-1")
            .rhash_hex = "ab".repeat(32);

        let report = evaluate(&mesh, &snapshot, &names);
        assert!(report.findings.iter().any(|f| matches!(
            f,
            Finding::IdentityMismatch { node, .. } if node == "sentinel-1"
        )));
    }

    /// A snapshot naming a node this topology never generated — two clusters'
    /// outputs merged by accident must not quietly verify.
    #[test]
    fn a_snapshot_for_a_foreign_node_is_refused_not_ignored() {
        let (mesh, _, names) = fixture(TopologyMode::FullMesh);
        let mut snapshot = complete_snapshot(&mesh);
        snapshot.nodes.push(NodeSnapshot {
            name: "sentinel-99".to_string(),
            rhash_hex: "cd".repeat(32),
            buckets: BTreeMap::new(),
        });

        let report = evaluate(&mesh, &snapshot, &names);
        assert!(!report.is_complete());
        assert!(report.findings.iter().any(|f| matches!(
            f,
            Finding::UnknownPeer { observer, .. } if observer == "sentinel-99"
        )));
    }

    /// A snapshot round-trips through the format a collector will write.
    #[test]
    fn snapshots_round_trip_through_json() {
        let (mesh, _, _) = fixture(TopologyMode::FullMesh);
        let snapshot = complete_snapshot(&mesh);
        let json = serde_json::to_vec(&snapshot).expect("serialize");
        let back: Snapshot = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back.nodes.len(), snapshot.nodes.len());
        assert_eq!(back.collected_at.as_deref(), Some("fixture"));
    }
}
