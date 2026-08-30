use std::fs;
use std::io::Error;
use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;
use std::sync::Arc;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::crypto::AsymCryptoProvider;
use crate::encoding;
use crate::environment::{CostModel, EnvironmentMetadata, EnvironmentMetadataSpec};
use crate::rns::config_builder::DEFAULT_UDP_PORT;
use crate::rns::identity::NodeIdentity;
use strum::IntoEnumIterator;
use crate::node::{NodeBootstrapError, NodeRegistryType, NodeType, NodeTypeConfig};

pub trait IsConfiguration {
    fn is_for_testing(&self) -> bool;
}

/// A bootstrap peer: a node we start with a direct link to. The 64-byte
/// RNS public key is given in hex; the rhash (16-byte truncated SHA-256 of
/// the public key) is derived, never configured.
#[derive(Clone, Serialize, Deserialize)]
pub struct BootstrapPeer {
    /// Hex-encoded 64-byte RNS public key of the peer.
    pub public_key: String,
    pub ip: String,
    pub port: u16,
}

#[derive(Clone)]
pub struct Config {
    pub public_key: Vec<u8>,
    pub ip_address: IpAddr,
    pub rest_api_version: usize,
    pub node_type: NodeType,
    pub node_registry_types: Vec<NodeRegistryType>,
    pub main_environment_id: String,
    pub reconciliation_partition_id: String,
    pub environment_metadata: Arc<DashMap<String, EnvironmentMetadata>>,
    pub type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
    /// Persistent node identity (RNS keypair + Ed25519 signing key).
    pub identity: Arc<NodeIdentity>,
    /// Transport rhash of this node (truncated hash of its RNS public key).
    pub rhash: [u8; 16],
    /// Peers to link at boot (one UDP interface per peer).
    pub bootstrap_peers: Vec<BootstrapPeer>,
    /// Own listen UDP port for the RNS transport.
    pub rns_port: u16,
    /// Relay/gateway mode: re-announce and forward traffic for transitive
    /// discovery. `false` for leaves.
    pub transport_enabled: bool
}

impl Config {
    const CONFIG_FILE_LOCATION: &'static str = "config.json";
    const ENV_FILE_LOCATION: &'static str = "/env";

    pub fn build() -> Result<Config, NodeBootstrapError> {
        let spec = match Config::load_spec() {
            Ok(result) => result,
            Err(err) => return Err(NodeBootstrapError::from_io_error(err))
        };

        // build up environment metadata
        let environment_metadata = match Config::get_environment_metadata() {
            Ok(result) => result,
            Err(err) => return Err(NodeBootstrapError::from_io_error(err))
        };

        // Select node registry types based on node type.
        // Full nodes participate in all registries; light nodes participate in core registries.
        let node_registry_types = Self::default_node_registry_types(spec.is_full_node);

        // Populate per-type configurations (min/max connections + minimum stake).
        // These values are protocol-level defaults; real chain state and stake data
        // may refine them at runtime.
        //
        // The defaults seed every type at the uniform floor; the environment
        // spec may then override a type's minimum via `CostModel.per_type_min_stake`
        // (Phase 4.4). Env specs without that field (the common case) apply no
        // override and stay at the uniform default. Config-schema, not a wire change.
        let mut type_configs = Self::default_type_configs();
        if let Some(main_env) = environment_metadata.get(&spec.main_env_id) {
            for node_type in NodeRegistryType::iter() {
                if let Some(&per_type) = main_env.cost_model.per_type_min_stake.get(&node_type) {
                    if let Some(mut tc) = type_configs.get_mut(&node_type) {
                        tc.min_stake = per_type;
                    }
                }
            }
        }
        let type_configs = Arc::new(type_configs);

        // Load (or create) the persistent identity keystore. A corrupt
        // keystore is a hard error — silently regenerating would orphan
        // the node's stake under a new identity.
        let identity_path = spec.identity_path.clone().unwrap_or_else(|| "node_identity.json".to_string());
        let identity = Arc::new(match NodeIdentity::load_or_create(Path::new(&identity_path)) {
            Ok(identity) => identity,
            Err(e) => {
                return Err(NodeBootstrapError {
                    message: format!("failed to load node identity from {}: {}", identity_path, e),
                })
            }
        });
        let public_key = identity
            .ed25519
            .public_key()
            .map_err(|e| NodeBootstrapError {
                message: format!("failed to read ed25519 public key: {}", e),
            })?;

        eprintln!(
            "[pneumatic] node identity rhash={:02x?} ed25519={:?} rns_public_key={:?}",
            identity.rhash,
            hex::encode(&public_key),
            hex::encode(identity.rns.get_public_key().unwrap_or([0u8; 64]))
        );

        let rhash = identity.rhash;
        Ok(Config {
            public_key,
            ip_address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            rest_api_version: spec.rest_api_version,
            node_type: if spec.is_full_node { NodeType::Full } else { NodeType::Light },
            node_registry_types,
            environment_metadata,
            main_environment_id: spec.main_env_id,
            reconciliation_partition_id: spec.reconciliation_partition_id,
            type_configs,
            identity,
            rhash,
            bootstrap_peers: spec.bootstrap_peers.clone(),
            rns_port: spec.rns_port.unwrap_or(DEFAULT_UDP_PORT),
            transport_enabled: spec.transport_enabled,
        })
    }

    fn load_spec() -> Result<ConfigSpec, Error> {
        let file_read = &match fs::read(Self::CONFIG_FILE_LOCATION) {
            Ok(r) => r,
            Err(e) => return Err(e)
        };

        encoding::deserialize_json_to::<ConfigSpec>(file_read)
    }

    fn get_environment_metadata() -> Result<Arc<DashMap<String, EnvironmentMetadata>>, Error> {
        let mut env_specs = vec![];
        for file in fs::read_dir(Self::ENV_FILE_LOCATION)? {
            let file_path_buf = file?.path();
            let file_path = file_path_buf.as_path();
            let env_file_read = &match fs::read(file_path) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Could not load file {:?} as environment spec", file_path);
                    return Err(e);
                }
            };

            if env_file_read.len() > 0 {
                match encoding::deserialize_json_to::<EnvironmentMetadataSpec>(env_file_read) {
                    Ok(r) => env_specs.push(r),
                    Err(e) => {
                        eprintln!("Could not load file {:?} as environment spec", file_path);
                        return Err(e);
                    }
                }
            }
        }
        let mut environment_metadata = DashMap::new();
        for env_spec in env_specs {
            // Phase 5.7 / H6: reject specs whose security-relevant config is
            // outside the valid range (quorum percentages, max_risk, admin tax,
            // gas multipliers, shard count). A bad spec fails the node here —
            // surfaced as an io error so Config::build() fails boot — instead of
            // silently neutering finalization quorum or the risk gate.
            if let Err(validation_error) = env_spec.validate() {
                eprintln!("Could not load environment spec as valid: {validation_error}");
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    validation_error.to_string(),
                ));
            }
            let env_metadata = EnvironmentMetadata::load_from_spec(env_spec);
            environment_metadata.insert(env_metadata.environment_id.clone(), env_metadata);
        }

        Ok(Arc::new(environment_metadata))
    }

    pub fn get_max_node_number(&self, node_type: &NodeRegistryType) -> usize {
        match self.type_configs.get(node_type) {
            Some(node) => node.max,
            None => 0
        }
    }

    pub fn get_min_type_stake(&self, node_type: &NodeRegistryType) -> u64 {
        match self.type_configs.get(node_type) {
            Some(config) => config.min_stake,
            None => Self::default_min_stake()
        }
    }

    /// The protocol-level global minimum stake for the main environment, from
    /// `CostModel.global_min_stake`. Falls back to the cost-model default (10)
    /// when the environment is absent from the registry (e.g. tests). This is
    /// the second floor on top of the per-type minimum — a node must meet
    /// *both* to register or act (see `meets_minimum_stake`, and the
    /// `ActionRouter::check_stake` reference at `action_router.rs:180`).
    pub fn get_global_min_stake(&self) -> u64 {
        self.environment_metadata
            .get(&self.main_environment_id)
            .map(|env| env.cost_model.global_min_stake)
            .unwrap_or_else(CostModel::default_global_min_stake)
    }

    /// Return the node registry types this node participates in.
    ///
    /// Full nodes participate in all five registry types (Committer, Sentinel,
    /// Executor, Finalizer, Archiver) so they can broadcast to and receive
    /// from any peer type. Light nodes only participate in core registries
    /// (Committer, Sentinel, Executor, Finalizer) to reduce network overhead.
    fn default_node_registry_types(is_full_node: bool) -> Vec<NodeRegistryType> {
        if is_full_node {
            NodeRegistryType::iter().collect()
        } else {
            vec![
                NodeRegistryType::Committer,
                NodeRegistryType::Sentinel,
                NodeRegistryType::Executor,
                NodeRegistryType::Finalizer,
            ]
        }
    }

    /// Build per-type configurations with protocol-level defaults.
    ///
    /// Each `NodeTypeConfig` specifies the minimum/maximum number of registered
    /// nodes for that type and the minimum stake required to join that registry.
    /// These values represent the staking protocol's baseline — chain state and
    /// real stake data may adjust them at runtime.
    fn default_type_configs() -> DashMap<NodeRegistryType, NodeTypeConfig> {
        let stake = Self::default_min_stake();
        let configs = DashMap::new();
        for node_type in NodeRegistryType::iter() {
            configs.insert(
                node_type,
                NodeTypeConfig {
                    min: 1,
                    max: 1000,
                    min_stake: stake,
                },
            );
        }
        configs
    }

    fn default_min_stake() -> u64 {
        10
    }

    /// Build a Config for unit tests without reading from disk. Uses an
    /// ephemeral in-memory identity (no keystore file).
    pub fn new_for_testing(
        main_environment_id: String,
        environment_metadata: Arc<DashMap<String, EnvironmentMetadata>>,
        type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
    ) -> Self {
        let identity = NodeIdentity::generate_in_memory();
        let public_key = identity.ed25519.public_key().unwrap_or_default();
        let rhash = identity.rhash;
        Config {
            public_key,
            ip_address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![],
            main_environment_id,
            reconciliation_partition_id: String::from("default"),
            environment_metadata,
            type_configs,
            identity: Arc::new(identity),
            rhash,
            bootstrap_peers: Vec::new(),
            rns_port: DEFAULT_UDP_PORT,
            transport_enabled: false,
        }
    }
}

/// Whether `stake` meets the *lower* of the two registered floors.
///
/// Registration and action stake gates enforce a conjunction: a key must
/// satisfy both the protocol-level global minimum and the per-type minimum.
/// `meets_minimum_stake` returns `true` only when `stake` is at or above both —
/// mirroring the AND that `ActionRouter::check_stake` enforces at
/// (`action_router.rs:186-196`) and that the `StakeIndex` registration gate
/// (`node/stake_index.rs`) applies on the off-thread hot path. A `stake` below
/// either floor fails. Kept as a free function so it is reachable from any
/// module via `crate::config::meets_minimum_stake` (an inherent method on
/// `Config` would only be reachable via `Config::meets_minimum_stake`).
pub fn meets_minimum_stake(stake: u64, global_min: u64, type_min: u64) -> bool {
    stake >= global_min && stake >= type_min
}

#[derive(Serialize, Deserialize)]
pub struct ConfigSpec {
    #[serde(default)]
    public_key: Vec<u8>,
    is_full_node: bool,
    rest_api_version: usize,
    balance: u64,
    environments: Vec<String>,
    main_env_id: String,
    reconciliation_partition_id: String,
    /// Keystore file path (default `node_identity.json`).
    #[serde(default)]
    identity_path: Option<String>,
    /// Peers to link at boot.
    #[serde(default)]
    bootstrap_peers: Vec<BootstrapPeer>,
    /// Own listen UDP port (default 4242).
    #[serde(default)]
    rns_port: Option<u16>,
    /// Relay/gateway mode (default false = leaf).
    #[serde(default)]
    transport_enabled: bool
}