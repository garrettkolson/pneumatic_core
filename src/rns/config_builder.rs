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
    pub fn new() -> Self {
        RnsNodeConfigBuilder {
            listen_ip: "127.0.0.1".to_string(),
            udp_port: DEFAULT_UDP_PORT,
            peers: Vec::new(),
            transport_enabled: false,
        }
    }

    pub fn with_listen_ip(mut self, ip: impl Into<String>) -> Self {
        self.listen_ip = ip.into();
        self
    }

    pub fn with_udp_port(mut self, port: u16) -> Self {
        self.udp_port = port;
        self
    }

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
