//! Node set, peer graph, and the RNS port arithmetic that only a central
//! generator can compute.
//!
//! ## The j-rule, and why it needs a generator
//!
//! rns-net's UDP interfaces are point-to-point. A node with P peers binds P
//! listen ports, `base + 0 ..= base + P-1`, and interface `k` forwards to peer
//! `k`. But the port it forwards to is **not** the peer's base port: the peer's
//! interface *for us* listens on `peer_base + j`, where `j` is our index in the
//! peer's peer list. Forward to `peer_base` instead and the peer receives our
//! packets on the wrong interface and the link handshake fails silently — no
//! error, no route, no traffic. (Documented from the failure itself in
//! `tests/pipeline_integration.rs:413-426`, and reproduced in
//! `tests/peered_topology_e2e.rs:206-215`.)
//!
//! `j` is a property of *another node's* configuration. A node cannot derive it
//! locally; the two sides have to be emitted together. That is this module's
//! reason to exist.
//!
//! ## The peer graph
//!
//! `FullMesh` links every pair. `RoleGraph` links only the pairs the protocol
//! actually sends across, read off the fan-out call sites — see [`FANOUT`]. It
//! is worth stating plainly what that buys: the pipeline's send graph is a full
//! mesh **minus executor↔executor**, so at equal role counts `RoleGraph` removes
//! exactly one of the ten role-pair classes. The generator emits both and prints
//! the real per-node interface cost, rather than implying role-graph is a
//! density fix.

use serde::{Deserialize, Serialize};

/// A node's role. Kept independent of `pneumatic_core::NodeRegistryType` because
/// the generator runs before any node exists and needs to serialize this into a
/// manifest; the mapping to the registry type is [`Role::registry_type_str`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Sentinel,
    Executor,
    Finalizer,
    Committer,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Sentinel, Role::Executor, Role::Finalizer, Role::Committer];

    /// The `NodeRegistryType` variant name this role is registered under. Kept
    /// as a string because it only has to match the registry's serde spelling.
    pub fn registry_type_str(&self) -> &'static str {
        match self {
            Role::Sentinel => "Sentinel",
            Role::Executor => "Executor",
            Role::Finalizer => "Finalizer",
            Role::Committer => "Committer",
        }
    }

    pub fn plural(&self) -> &'static str {
        match self {
            Role::Sentinel => "sentinels",
            Role::Executor => "executors",
            Role::Finalizer => "finalizers",
            Role::Committer => "committers",
        }
    }
}

/// Who sends to whom, read off the `send_to_all` call sites on 10/02/2026:
///
/// | Sender | Targets | Evidence |
/// |---|---|---|
/// | Sentinel | Executor, Finalizer, Sentinel | `sentinel/src/transaction_notifier.rs:42,105,137,152,167,186` |
/// | Executor | Finalizer | `executor/src/executor.rs:867,904` |
/// | Finalizer | Committer, Finalizer, Sentinel, Executor | `finalizer/src/message_dispatcher.rs:71,105,133,166-175,203-209,239-245` |
/// | Committer | Committer, Executor, Sentinel | `committer/src/committer/quoruming.rs:136-139,164-167`, `committer/src/block_services.rs:135,167` |
///
/// Two things fall out that are easy to get wrong by intuition. Nobody fans out
/// to *executor from executor*, so intra-executor links are the only class the
/// role graph can drop. And the committer never sends to `Finalizer` — finalizers
/// learn about commits by other routes — yet a `Finalizer → Committer` send
/// still means the *link* between them is required, because RNS links are
/// bidirectional.
pub const FANOUT: [(Role, &[Role]); 4] = [
    (Role::Sentinel, &[Role::Executor, Role::Finalizer, Role::Sentinel]),
    (Role::Executor, &[Role::Finalizer]),
    (
        Role::Finalizer,
        &[Role::Committer, Role::Finalizer, Role::Sentinel, Role::Executor],
    ),
    (Role::Committer, &[Role::Committer, Role::Executor, Role::Sentinel]),
];

/// True if some node of role `from` sends to a node of role `to`.
pub fn role_sends(from: Role, to: Role) -> bool {
    FANOUT
        .iter()
        .find(|(role, _)| *role == from)
        .map(|(_, targets)| targets.contains(&to))
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TopologyMode {
    /// Every pair linked.
    FullMesh,
    /// Only pairs the protocol actually sends across, in either direction.
    RoleGraph,
    /// Full mesh while the cluster is small, role graph beyond a threshold.
    /// A distinct mode rather than a rule attached to `FullMesh`, so that
    /// asking for a full mesh always gets one.
    Auto,
}

impl TopologyMode {
    /// Resolve `Auto` into a concrete topology for a cluster of `total` nodes.
    /// The other two variants are answers, not questions: `FullMesh` stays
    /// `FullMesh` at any size, because an operator who asked for the dense case
    /// and quietly got a sparser one would be debugging the wrong graph.
    pub fn resolve(self, total_nodes: usize, role_graph_above: usize) -> TopologyMode {
        match self {
            TopologyMode::Auto if total_nodes > role_graph_above => TopologyMode::RoleGraph,
            TopologyMode::Auto => TopologyMode::FullMesh,
            concrete => concrete,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    pub index: usize,
    pub name: String,
    pub role: Role,
}

/// One node's identity in the mesh, plus the transport address it must be
/// reached on.
#[derive(Clone, Debug)]
pub struct NodePlan {
    pub node: Node,
    /// This node's base UDP port; its interfaces occupy
    /// `base_port .. base_port + interfaces`.
    pub base_port: u16,
    /// The address peers dial to reach this node.
    ///
    /// This lands in *other* nodes' `bootstrap_peers[].ip`, never in this
    /// node's own config. A node's own address is a bind decision
    /// (`Config::rns_listen_ip`), not a peer entry — mixing the two produces a
    /// node that announces itself to itself.
    pub address: String,
    /// Peers in interface order. `bootstrap_peers` and the transport builder
    /// must both consume this exact ordering, because `j` is defined by it.
    pub peers: Vec<usize>,
}

impl NodePlan {
    /// The port a peer must forward to in order to reach *this* node: our base
    /// plus the requester's index in our peer list.
    pub fn listen_port_for(&self, requester_index: usize) -> Result<u16, String> {
        let j = self
            .peers
            .iter()
            .position(|&p| p == requester_index)
            .ok_or_else(|| {
                format!(
                    "topology is not symmetric: node {} is not a peer of {}",
                    requester_index, self.node.index
                )
            })?;
        Ok(self.base_port + j as u16)
    }
}

/// Where the nodes actually run.
///
/// One concept rather than two flags, because "where" decides two coupled
/// questions at once: whether port windows must be disjoint, and which address
/// a peer dials. Keeping them in one value is what makes "one node per machine
/// but everyone dials 127.0.0.1" — a cluster where nothing can connect —
/// unrepresentable.
#[derive(Clone, Debug)]
pub enum Placement {
    /// Every node on one host (development, CI). Windows must be disjoint or two
    /// nodes bind the same UDP port; every peer is dialed at loopback.
    SingleHost,
    /// One node per machine. Windows may overlap because the address
    /// disambiguates, so all nodes share a base port and each host exposes one
    /// range. `addresses` is one entry per node, in plan order.
    ///
    /// Addresses are stored as text, not parsed: a cloud provisioner hands back
    /// private DNS names as readily as IPs, and the transport takes a string.
    /// Nothing here needs to interpret the value, so nothing here should be able
    /// to reject it wrongly.
    PerHost { addresses: Vec<String> },
}

impl Placement {
    const LOOPBACK: &'static str = "127.0.0.1";

    /// The address peers dial to reach node `index`. Validated against the node
    /// count rather than defaulted: a short address list silently deploying a
    /// cluster of loopback entries is precisely the quiet misconfiguration this
    /// generator exists to prevent.
    pub fn address_for(&self, index: usize, node_count: usize) -> Result<String, String> {
        match self {
            Placement::SingleHost => Ok(Placement::LOOPBACK.to_string()),
            Placement::PerHost { addresses } => {
                if addresses.len() != node_count {
                    return Err(format!(
                        "per-host placement needs exactly one address per node: got {}, need {}",
                        addresses.len(),
                        node_count
                    ));
                }
                let address = addresses[index].trim();
                if address.is_empty() {
                    return Err(format!("the address for node {index} is empty"));
                }
                Ok(address.to_string())
            }
        }
    }

    /// Short stable name for manifests and reports.
    pub fn label(&self) -> &'static str {
        match self {
            Placement::SingleHost => "single-host",
            Placement::PerHost { .. } => "per-host",
        }
    }

    /// Whether each node needs its own block of ports.
    pub fn windows_must_be_disjoint(&self) -> bool {
        matches!(self, Placement::SingleHost)
    }
}

/// The generated mesh: nodes, their peer lists, and their port windows.
#[derive(Clone, Debug)]
pub struct Mesh {
    pub plan: Vec<NodePlan>,
    pub mode: TopologyMode,
    /// UDP ports reserved per node: interfaces plus slack, so windows never
    /// overlap and a peer's forwarded port is always inside the target's block.
    pub port_window: u16,
    /// Recorded so that a manifest can say how the cluster was meant to run.
    pub placement: Placement,
}

/// How many UDP ports a node's window reserves. rns-net binds one listen socket
/// per peer, and the existing harnesses over-allocate rather than sit on the
/// boundary (`pipeline_integration.rs` reserves 8 per node), because the window
/// also holds the ports peers forward *into*.
const PORT_SLACK: u16 = 2;

impl Mesh {
    /// Build the mesh. `counts` is `(sentinels, executors, finalizers, committers)`.
    ///
    /// Fails closed on an empty role set or on a peer graph that is not
    /// symmetric — asymmetry would make `j` unresolvable for one direction, and
    /// the failure mode in the field is a silently dead route.
    pub fn build(
        counts: [(Role, usize); 4],
        base_port: u16,
        mode: TopologyMode,
        placement: Placement,
    ) -> Result<Mesh, String> {
        let mut nodes = Vec::new();
        for (role, count) in counts {
            if count == 0 {
                continue;
            }
            for i in 0..count {
                nodes.push(Node {
                    index: nodes.len(),
                    name: format!("{}-{}", role.plural().trim_end_matches('s'), i + 1),
                    role,
                });
            }
        }
        if nodes.is_empty() {
            return Err("no nodes requested: every role count is zero".to_string());
        }
        // One role alone is a cluster of one with nobody to peer with.
        if nodes.len() == 1 {
            return Err("a testnet needs at least two nodes".to_string());
        }

        let mut plan: Vec<NodePlan> = nodes
            .iter()
            .map(|node| NodePlan {
                node: node.clone(),
                base_port: 0,
                address: String::new(),
                peers: Vec::new(),
            })
            .collect();

        for i in 0..plan.len() {
            for j in 0..plan.len() {
                if i == j || plan[i].peers.contains(&j) {
                    continue;
                }
                let linked = match mode {
                    TopologyMode::FullMesh => true,
                    TopologyMode::Auto => {
                        return Err(
                            "Auto must be resolved to a concrete topology before building                              a mesh (see TopologyMode::resolve)".to_string(),
                        )
                    }
                    TopologyMode::RoleGraph => {
                        role_sends(plan[i].node.role, plan[j].node.role)
                            || role_sends(plan[j].node.role, plan[i].node.role)
                    }
                };
                if linked {
                    plan[i].peers.push(j);
                }
            }
            // Interface order is the peer order; `j` is defined by it, so it
            // must be deterministic and identical on both sides.
            plan[i].peers.sort_unstable();
        }

        // Symmetry: RNS links are bidirectional, so a one-sided peer entry would
        // make `listen_port_for` unresolvable for one direction.
        for node in &plan {
            for &peer in &node.peers {
                if !plan[peer].peers.contains(&node.node.index) {
                    return Err(format!(
                        "topology is not symmetric: {} lists {} but not the reverse",
                        node.node.name, plan[peer].node.name
                    ));
                }
            }
        }

        // Port windows must not overlap between nodes, or two nodes bind the same
        // socket and one boots "without transport" (the failure mode the
        // composite had until 10/02/2026).
        let max_peers = plan.iter().map(|p| p.peers.len()).max().unwrap_or(0);
        if max_peers == 0 {
            return Err(format!(
                "{mode:?} topology produced no links for these role counts"
            ));
        }
        let window = max_peers as u16 + PORT_SLACK;

        // Addresses resolve first, and fail the build if the count is wrong:
        // a partially-assigned cluster is worse than no cluster.
        let mut addresses = Vec::with_capacity(plan.len());
        for i in 0..plan.len() {
            addresses.push(placement.address_for(i, plan.len())?);
        }

        if placement.windows_must_be_disjoint() {
            let span = (plan.len() as u32) * (window as u32);
            let highest = base_port as u32 + span;
            if highest > u16::MAX as u32 {
                return Err(format!(
                    "port window overflows u16: base {base_port} + {span} ports"
                ));
            }
            for (i, node) in plan.iter_mut().enumerate() {
                node.base_port = base_port + (i as u16) * window;
            }
        } else {
            // One machine per node. The address distinguishes nodes, so every
            // host runs the same range: `base + j` where `j` is the sender's
            // index in the target's peer list. The firewall consequence is the
            // point — one UDP range per instance instead of one per node.
            if base_port as u32 + window as u32 > u16::MAX as u32 {
                return Err(format!(
                    "port window overflows u16 on a single host: base {base_port} + {window} ports"
                ));
            }
            for node in plan.iter_mut() {
                node.base_port = base_port;
            }
        }
        for (node, address) in plan.iter_mut().zip(addresses) {
            node.address = address;
        }

        Ok(Mesh { plan, mode, port_window: window, placement })
    }

    /// Rebuild the plan recorded in a generated `manifest.json`.
    ///
    /// A probe or launcher judging a *running* cluster must judge it against the
    /// topology it was actually built from. Re-deriving from the flags typed
    /// today would silently disagree with the cluster on disk — the classic
    /// "200 edges missing" report that really means "different counts were used".
    /// The manifest is the record of what was generated, so this is the reader.
    pub fn from_manifest(manifest: &serde_json::Value) -> Result<Mesh, String> {
        let mode: TopologyMode = serde_json::from_value(manifest["mode"].clone())
            .map_err(|e| format!("manifest mode is unreadable: {e}"))?;
        let base_port = manifest["base_port"]
            .as_u64()
            .ok_or("manifest base_port missing")? as u16;
        let placement_label = manifest["placement"].as_str().unwrap_or("single-host");
        let nodes = manifest["nodes"].as_array().ok_or("manifest.nodes missing")?;
        if nodes.is_empty() {
            return Err("manifest lists no nodes".to_string());
        }

        // First pass: identity and index, so peer names can be resolved.
        let mut plan: Vec<NodePlan> = Vec::with_capacity(nodes.len());
        let mut by_name: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for node in nodes {
            let role: Role = serde_json::from_value(node["role"].clone())
                .map_err(|e| format!("manifest node role unreadable: {e}"))?;
            let name = node["name"].as_str().ok_or("manifest node missing name")?.to_string();
            if by_name.insert(name.clone(), plan.len()).is_some() {
                return Err(format!("manifest lists {name} twice"));
            }
            plan.push(NodePlan {
                node: Node { index: plan.len(), name, role },
                base_port: node["rns_port"].as_u64().ok_or("manifest node missing rns_port")?
                    as u16,
                address: node["address"].as_str().unwrap_or("127.0.0.1").to_string(),
                peers: Vec::new(),
            });
        }

        // Second pass: peer names to indices. An unresolvable name means the
        // manifest was edited or truncated, so it fails rather than quietly
        // dropping edges the probe would then report as missing.
        for (i, node) in nodes.iter().enumerate() {
            let peers = node["peers"].as_array().ok_or("manifest node missing peers")?;
            for peer in peers {
                let peer_name = peer.as_str().ok_or("peer entry is not a name")?;
                let index = *by_name
                    .get(peer_name)
                    .ok_or_else(|| format!("{} lists unknown peer {peer_name}", plan[i].node.name))?;
                plan[i].peers.push(index);
            }
        }

        let placement = match placement_label {
            "per-host" => Placement::PerHost {
                addresses: plan.iter().map(|p| p.address.clone()).collect(),
            },
            _ => Placement::SingleHost,
        };
        let port_window = match manifest["ports_per_host"].as_u64() {
            Some(w) => w as u16,
            None => plan
                .iter()
                .map(|p| p.peers.len())
                .max()
                .ok_or("no nodes")? as u16
                + PORT_SLACK,
        };

        Ok(Mesh { plan, mode, port_window, placement })
    }

    /// UDP ports one machine must expose for its node — the number that sizes a
    /// security group. Distinct from `ports_reserved`, which counts the whole
    /// cluster's port space and is only meaningful on a single host.
    pub fn ports_per_host(&self) -> u16 {
        self.port_window
    }

    /// UDP ports reserved across the cluster's port space.
    pub fn ports_reserved(&self) -> u32 {
        if self.placement.windows_must_be_disjoint() {
            (self.plan.len() as u32) * (self.port_window as u32)
        } else {
            // The same range is reused on every machine, so the distinct port
            // space is one window even for a 40-node fleet.
            self.port_window as u32
        }
    }

    /// Interfaces this node binds — one per peer.
    pub fn interfaces(&self, index: usize) -> usize {
        self.plan[index].peers.len()
    }

    pub fn max_interfaces(&self) -> usize {
        self.plan.iter().map(|p| p.peers.len()).max().unwrap_or(0)
    }

    pub fn by_role(&self, role: Role) -> Vec<usize> {
        self.plan
            .iter()
            .filter(|p| p.node.role == role)
            .map(|p| p.node.index)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(s: usize, e: usize, f: usize, c: usize) -> [(Role, usize); 4] {
        [(Role::Sentinel, s), (Role::Executor, e), (Role::Finalizer, f), (Role::Committer, c)]
    }

    #[test]
    fn full_mesh_links_every_pair() {
        let mesh = Mesh::build(counts(2, 2, 2, 2), 5000, TopologyMode::FullMesh, Placement::SingleHost).expect("build");
        assert_eq!(mesh.plan.len(), 8);
        assert_eq!(mesh.max_interfaces(), 7);
        for node in &mesh.plan {
            assert_eq!(node.peers.len(), mesh.plan.len() - 1);
        }
    }

    /// The finding worth pinning: the protocol's send graph is a full mesh minus
    /// executor↔executor. If someone "optimizes" the role graph further, or the
    /// fan-out call sites change, this fails and forces a re-read of `FANOUT`.
    #[test]
    fn role_graph_drops_only_intra_executor_links() {
        let mesh = Mesh::build(counts(2, 2, 2, 2), 5000, TopologyMode::RoleGraph, Placement::SingleHost).expect("build");
        let full = Mesh::build(counts(2, 2, 2, 2), 5000, TopologyMode::FullMesh, Placement::SingleHost).expect("build");

        // Unordered role pairs, including same-role pairs: intra-role links are
        // exactly what the role graph is allowed to drop.
        let missing: Vec<(Role, Role)> = Role::ALL
            .iter()
            .enumerate()
            .flat_map(|(i, a)| {
                Role::ALL.iter().skip(i).map(move |b| (*a, *b))
            })
            .filter(|(a, b)| !role_sends(*a, *b) && !role_sends(*b, *a))
            .collect();
        assert_eq!(
            missing,
            vec![(Role::Executor, Role::Executor)],
            "role graph must differ from full mesh only by intra-executor links"
        );

        // Executors alone feel the difference: they lose their intra-role links.
        for node in &mesh.plan {
            let expected = if node.node.role == Role::Executor {
                full.interfaces(node.node.index) - 1
            } else {
                full.interfaces(node.node.index)
            };
            assert_eq!(mesh.interfaces(node.node.index), expected, "{}", node.node.name);
        }
    }

    #[test]
    fn port_windows_never_overlap() {
        let mesh = Mesh::build(counts(2, 2, 2, 2), 20_000, TopologyMode::FullMesh, Placement::SingleHost).expect("build");
        for a in &mesh.plan {
            for b in &mesh.plan {
                if a.node.index == b.node.index {
                    continue;
                }
                // A's block covers base ..= base + interfaces - 1, and peers may
                // forward into any of those; windows must be disjoint.
                let a_end = a.base_port + a.peers.len() as u16;
                let b_end = b.base_port + b.peers.len() as u16;
                assert!(
                    a.base_port > b_end || b.base_port > a_end || a.base_port == b.base_port,
                    "windows overlap: {}@{}..{} vs {}@{}..{}",
                    a.node.name, a.base_port, a_end, b.node.name, b.base_port, b_end
                );
            }
        }
    }

    /// The j-rule, checked in both directions for every edge: if A forwards to
    /// `B_base + j`, then B's interface `j` must be the one A is on.
    #[test]
    fn every_edge_resolves_both_ways() {
        for mode in [TopologyMode::FullMesh, TopologyMode::RoleGraph] {
            let mesh = Mesh::build(counts(1, 2, 2, 1), 30_000, mode, Placement::SingleHost).expect("build");
            for a in &mesh.plan {
                for &peer in &a.peers {
                    let b = &mesh.plan[peer];
                    // Where A must forward to reach B: B's listen port for A.
                    let port_a_to_b = b.listen_port_for(a.node.index).unwrap_or_else(|e| {
                        panic!("{mode:?} {}->{}: {e}", a.node.name, b.node.name)
                    });
                    // ...and the reverse.
                    let port_b_to_a = a.listen_port_for(b.node.index).expect("symmetric");

                    // Each must land inside the other's bound interface range.
                    assert!(
                        port_a_to_b >= b.base_port && port_a_to_b < b.base_port + b.peers.len() as u16,
                        "{mode:?} {}→{}: forward port {port_a_to_b} outside {}'s interfaces {}..{}",
                        a.node.name, b.node.name, b.node.name, b.base_port,
                        b.base_port + b.peers.len() as u16
                    );
                    assert!(
                        port_b_to_a >= a.base_port && port_b_to_a < a.base_port + a.peers.len() as u16,
                        "{mode:?} {}→{}: forward port {port_b_to_a} outside {}'s interfaces",
                        b.node.name, a.node.name, a.node.name
                    );

                    // And the two directions must be distinct ports, or the two
                    // nodes would collide on a socket.
                    assert_ne!(port_a_to_b, port_b_to_a, "{mode:?} {}↔{} collide on one port", a.node.name, b.node.name);
                }
            }
        }
    }

    #[test]
    fn empty_and_single_node_requests_fail_closed() {
        assert!(Mesh::build(counts(0, 0, 0, 0), 5000, TopologyMode::FullMesh, Placement::SingleHost).is_err());
        assert!(Mesh::build(counts(1, 0, 0, 0), 5000, TopologyMode::FullMesh, Placement::SingleHost).is_err());
    }

    #[test]
    fn unresolvable_peer_is_an_error_not_a_silent_default() {
        let mut mesh = Mesh::build(counts(1, 1, 1, 1), 5000, TopologyMode::FullMesh, Placement::SingleHost).expect("build");
        mesh.plan[1].peers.retain(|&p| p != 0);
        let err = mesh.plan[1].listen_port_for(0).expect_err("absent peer must not resolve");
        assert!(err.contains("not symmetric"), "unexpected error: {err}");
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    /// The bug this pins: `auto`'s threshold rule was once attached to
    /// `FullMesh`, so an explicit `--topology full-mesh` at 40 validators
    /// silently produced a role graph. The operator would then be reasoning
    /// about a graph they never asked for.
    #[test]
    fn explicit_modes_are_answers_at_any_size() {
        for total in [2usize, 8, 40, 200] {
            assert_eq!(TopologyMode::FullMesh.resolve(total, 8), TopologyMode::FullMesh);
            assert_eq!(TopologyMode::RoleGraph.resolve(total, 8), TopologyMode::RoleGraph);
        }
    }

    #[test]
    fn auto_switches_at_its_threshold() {
        assert_eq!(TopologyMode::Auto.resolve(8, 8), TopologyMode::FullMesh);
        assert_eq!(TopologyMode::Auto.resolve(9, 8), TopologyMode::RoleGraph);
        // Threshold is operator-settable, including "never".
        assert_eq!(TopologyMode::Auto.resolve(1000, 1000), TopologyMode::FullMesh);
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;

    /// Mirrors the one in `tests::`; test modules don't share helpers.
    fn counts(sentinels: usize, executors: usize, finalizers: usize, committers: usize)
        -> [(Role, usize); 4]
    {
        [
            (Role::Sentinel, sentinels),
            (Role::Executor, executors),
            (Role::Finalizer, finalizers),
            (Role::Committer, committers),
        ]
    }

    fn per_host(count: usize) -> Placement {
        Placement::PerHost {
            addresses: (0..count).map(|i| format!("10.0.0.{}", i + 1)).collect(),
        }
    }

    /// One machine per node means the address distinguishes nodes, so a shared
    /// port range is correct — and the firewall consequence matters: one UDP
    /// range per instance rather than one per node.
    #[test]
    fn per_host_placement_shares_one_base_port() {
        let mesh = Mesh::build(counts(1, 1, 1, 1), 20_000, TopologyMode::FullMesh, per_host(4))
            .expect("build");
        for plan in &mesh.plan {
            assert_eq!(
                plan.base_port, 20_000,
                "{} must use the shared base",
                plan.node.name
            );
        }
        // Addresses follow plan order, so `--addresses` lines up with the report.
        for (i, plan) in mesh.plan.iter().enumerate() {
            assert_eq!(plan.address, format!("10.0.0.{}", i + 1));
        }
        // One window of distinct ports for the whole cluster, not one per node.
        assert_eq!(mesh.ports_reserved(), mesh.ports_per_host() as u32);
    }

    #[test]
    fn single_host_placement_keeps_windows_disjoint_and_dials_loopback() {
        let mesh = Mesh::build(counts(1, 1, 1, 1), 20_000, TopologyMode::FullMesh, Placement::SingleHost)
            .expect("build");
        let mut bases: Vec<u16> = mesh.plan.iter().map(|p| p.base_port).collect();
        bases.sort();
        bases.dedup();
        assert_eq!(bases.len(), 4, "one-host bases must not collide");
        for plan in &mesh.plan {
            assert_eq!(plan.address, "127.0.0.1");
        }
        assert_eq!(mesh.ports_reserved(), 4 * mesh.ports_per_host() as u32);
    }

    /// A short address list must fail the build, not pad out with loopback: a
    /// cluster where some peers dial 127.0.0.1 boots happily and connects to
    /// nobody, which is the quiet failure this generator exists to prevent.
    #[test]
    fn per_host_placement_requires_exactly_one_address_per_node() {
        let err = Mesh::build(
            counts(1, 1, 1, 1),
            20_000,
            TopologyMode::FullMesh,
            Placement::PerHost {
                addresses: vec!["10.0.0.1".into(), "10.0.0.2".into()],
            },
        )
        .expect_err("2 addresses for 4 nodes must not build");
        assert!(
            err.contains("2") && err.contains("4"),
            "the error must name both counts, got: {err}"
        );

        let blank = Mesh::build(
            counts(1, 1, 1, 1),
            20_000,
            TopologyMode::FullMesh,
            Placement::PerHost {
                addresses: vec!["10.0.0.1".into(), "  ".into(), "10.0.0.3".into(), "10.0.0.4".into()],
            },
        )
        .expect_err("a blank address must not build");
        assert!(blank.contains("empty"), "got: {blank}");
    }

    /// The j-rule survives a shared base port: peers are distinguished by
    /// address, and the forwarded port is still the target's `base + j`.
    #[test]
    fn per_host_edges_still_resolve_both_ways() {
        let mesh = Mesh::build(counts(2, 1, 1, 1), 20_000, TopologyMode::FullMesh, per_host(5))
            .expect("build");
        for plan in &mesh.plan {
            for &peer in &plan.peers {
                let target = &mesh.plan[peer];
                let forward = target
                    .listen_port_for(plan.node.index)
                    .expect("edge must resolve");
                assert!(
                    (target.base_port..target.base_port + mesh.ports_per_host()).contains(&forward),
                    "{} → {} forwards to {forward}, outside {}'s window",
                    plan.node.name,
                    target.node.name,
                    target.node.name
                );
            }
        }
    }
}
