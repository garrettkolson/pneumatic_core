//! `NodeConfig` builder — the single choke point for rns-net API churn.
//!
//! rns-net 0.7.0's `NodeConfig` has ~45 fields and no `Default`, so the full
//! literal lives here (ported from the Stage-0 spike). A version bump of
//! rns-net — pinned exactly in Cargo.toml — means re-migrating exactly this
//! file.
//!
//! Interface topology (verified in the Stage-0 spike): rns-net's UDP
//! interfaces are point-to-point — one forward target each — and every UDP
//! interface requires its OWN unique listen port. So a node with N bootstrap
//! peers gets N interfaces: interface `i` listens on `udp_port + i` and
//! forwards to peer `i`. A node with no peers gets one listener-only
//! interface on `udp_port`.
//!
//! No TCP interfaces in v1. Multicast is not an rns-net knob; announces
//! traverse established links only, which is exactly the permissioned
//! discovery behavior pneumatic wants.

use std::time::Duration;

use rns_crypto::identity::Identity;
use rns_net::{InterfaceConfig, InterfaceId, MODE_FULL, NodeConfig, UdpConfig};

const KNOWN_DESTINATIONS_TTL: Duration = Duration::from_secs(48 * 60 * 60);

/// Default own-listen UDP port for the RNS transport.
pub const DEFAULT_UDP_PORT: u16 = 4242;

/// Builder for rns-net 0.7.0's `NodeConfig`.
///
/// Deliberately exposes only the four knobs pneumatic actually varies (listen
/// IP, base UDP port, peer list, transport flag); everything else `build`
/// produces is a fixed, reviewed constant. New options must be added here —
/// call sites must not assemble `NodeConfig` themselves — so the whole
/// ~45-field literal stays in one file and a rns-net bump is a single-file
/// migration (see module docs and `rns/mod.rs`).
pub struct RnsNodeConfigBuilder {
    listen_ip: String,
    udp_port: u16,
    peers: Vec<(String, u16)>,
    transport_enabled: bool,
}

impl Default for RnsNodeConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RnsNodeConfigBuilder {
    /// New builder with the leaf-node defaults: listen on `127.0.0.1`, base UDP
    /// port [`DEFAULT_UDP_PORT`] (4242), no peers, `transport_enabled: false`
    /// (a leaf learns paths but does not route for others). The
    /// `Default` impl forwards here, so both entry points stay in sync.
    pub fn new() -> Self {
        RnsNodeConfigBuilder {
            listen_ip: "127.0.0.1".to_string(),
            udp_port: DEFAULT_UDP_PORT,
            peers: Vec::new(),
            transport_enabled: false,
        }
    }

    /// Bind address for every generated UDP interface. Default
    /// `127.0.0.1` (single-host mesh); multi-host deployments set the
    /// externally reachable address.
    pub fn with_listen_ip(mut self, ip: impl Into<String>) -> Self {
        self.listen_ip = ip.into();
        self
    }

    /// Base UDP listen port. Because each rns-net UDP interface needs its own
    /// unique port, a node with N peers occupies N consecutive ports starting
    /// here (`udp_port + i` for interface `i`; a peerless node uses just
    /// `udp_port`). Choose a base that leaves that span clear — and clear of
    /// the pneumatic TCP port table in `conns.rs`.
    pub fn with_udp_port(mut self, port: u16) -> Self {
        self.udp_port = port;
        self
    }

    /// Add a bootstrap peer, addressed by its *own* listen `ip:port` (the
    /// base port on their side, not a derived `+i` value — the `+i` rule
    /// applies to *this* node's listen sockets only). Interfaces are assigned
    /// in insertion order: peer `i` is the forward target of interface `i`.
    pub fn add_peer(mut self, ip: impl Into<String>, port: u16) -> Self {
        self.peers.push((ip.into(), port));
        self
    }

    /// `true` for relay/gateway nodes that must re-announce and forward
    /// traffic for transitive discovery; `false` (default) for leaves, which
    /// learn paths but are excluded from multi-hop routing.
    pub fn with_transport_enabled(mut self, enabled: bool) -> Self {
        self.transport_enabled = enabled;
        self
    }

    /// Build the full `NodeConfig` for `identity`.
    ///
    /// Interface generation follows the point-to-point rule from the module
    /// docs: one interface per peer (listen `udp_port + i`, forward to peer
    /// `i`), or a single listener-only interface when there are no peers.
    /// Interfaces are named `pneumatic-udp-{i}` with 1-based `InterfaceId`s.
    /// No TCP interfaces are emitted (v1 decision). Notable fixed choices
    /// baked into the literal: `transport_enabled` comes from the builder
    /// (default false = leaf), `known_destinations_ttl` is 48 h, ingress
    /// control is enabled everywhere, `max_paths_per_destination: 1` (no
    /// multipath in v1), and the identity is rebuilt from `identity`'s private
    /// key so the config holds its own instance rather than aliasing the
    /// caller's — `panic_on_interface_error`
    /// is false so a bad interface cannot take the node down.
    pub fn build(self, identity: &Identity) -> NodeConfig {
        let ifaces: Vec<(u16, Option<(String, u16)>)> = if self.peers.is_empty() {
            vec![(self.udp_port, None)]
        } else {
            self.peers
                .iter()
                .enumerate()
                .map(|(i, (ip, port))| (self.udp_port + i as u16, Some((ip.clone(), *port))))
                .collect()
        };

        let interfaces: Vec<InterfaceConfig> = ifaces
            .into_iter()
            .enumerate()
            .map(|(i, (listen_port, forward))| {
                let (forward_ip, forward_port) = match forward {
                    Some((ip, port)) => (Some(ip), Some(port)),
                    None => (None, None),
                };
                let config = UdpConfig {
                    name: format!("pneumatic-udp-{i}"),
                    listen_ip: Some(self.listen_ip.clone()),
                    listen_port: Some(listen_port),
                    forward_ip,
                    forward_port,
                    interface_id: InterfaceId((i + 1) as u64),
                    ..UdpConfig::default()
                };
                InterfaceConfig {
                    name: String::new(),
                    type_name: "UDPInterface".to_string(),
                    config_data: Box::new(config),
                    mode: MODE_FULL,
                    gravity: 0,
                    recursive_prs: false,
                    announces_from_internal: true,
                    announces_to_internal: None,
                    ingress_control: rns_core::transport::types::IngressControlConfig::enabled(),
                    ifac: None,
                    discovery: None,
                }
            })
            .collect();

        NodeConfig {
            panic_on_interface_error: false,
            transport_enabled: self.transport_enabled,
            static_transport_identity: false,
            local_hops_delta: false,
            identity: Some(Identity::from_private_key(
                &identity.get_private_key().unwrap(),
            )),
            interfaces,
            share_instance: false,
            instance_name: "default".into(),
            shared_instance_port: 37428,
            rpc_port: 0,
            cache_dir: None,
            ratchet_store: None,
            ratchet_expiry: Duration::from_secs(rns_core::constants::RATCHET_EXPIRY),
            management: Default::default(),
            probe_port: None,
            probe_addrs: vec![],
            probe_protocol: rns_core::holepunch::ProbeProtocol::Rnsp,
            device: None,
            hooks: Vec::new(),
            discover_interfaces: false,
            autoconnect_interface_mode: None,
            autoconnect_interface_gravity: 0,
            autoconnect_announces_to_internal: false,
            discovery_required_value: None,
            respond_to_probes: false,
            prefer_shorter_path: false,
            max_paths_per_destination: 1,
            packet_hashlist_max_entries: rns_core::constants::HASHLIST_MAXSIZE,
            packet_hashlist_allocation: rns_core::transport::types::PacketHashlistAllocation::Eager,
            max_discovery_pr_tags: rns_core::constants::MAX_PR_TAGS,
            max_path_destinations: usize::MAX,
            max_tunnel_destinations_total: usize::MAX,
            known_destinations_ttl: KNOWN_DESTINATIONS_TTL,
            known_destinations_max_entries: 8192,
            announce_table_ttl: Duration::from_secs(
                rns_core::constants::ANNOUNCE_TABLE_TTL as u64,
            ),
            announce_table_max_bytes: rns_core::constants::ANNOUNCE_TABLE_MAX_BYTES,
            driver_event_queue_capacity: rns_net::event::DEFAULT_EVENT_QUEUE_CAPACITY,
            interface_writer_queue_capacity: rns_net::interface::DEFAULT_ASYNC_WRITER_QUEUE_CAPACITY,
            announce_rate_defaults: rns_net::AnnounceRateDefaults::default(),
            ingress_control_defaults: rns_core::transport::types::IngressControlConfig::enabled(),
            backbone_peer_pool: None,
            announce_sig_cache_enabled: true,
            announce_sig_cache_max_entries: rns_core::constants::ANNOUNCE_SIG_CACHE_MAXSIZE,
            announce_sig_cache_ttl: Duration::from_secs(
                rns_core::constants::ANNOUNCE_SIG_CACHE_TTL as u64,
            ),
            registry: None,
        }
    }
}
