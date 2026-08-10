use std::fs;
use std::io::Error;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::encoding;
use crate::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
use strum::IntoEnumIterator;
use crate::node::{NodeBootstrapError, NodeRegistryType, NodeType, NodeTypeConfig};

pub trait IsConfiguration {
    fn is_for_testing(&self) -> bool;
}

pub struct Config {
    pub public_key: Vec<u8>,
    pub ip_address: IpAddr,
    pub rest_api_version: usize,
    pub node_type: NodeType,
    pub node_registry_types: Vec<NodeRegistryType>,
    pub main_environment_id: String,
    pub reconciliation_partition_id: String,
    pub environment_metadata: Arc<DashMap<String, EnvironmentMetadata>>,
    pub type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>
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
        let type_configs = Arc::new(Self::default_type_configs());

        Ok(Config {
            public_key: vec![],
            ip_address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            rest_api_version: spec.rest_api_version,
            node_type: if spec.is_full_node { NodeType::Full } else { NodeType::Light },
            node_registry_types,
            environment_metadata,
            main_environment_id: spec.main_env_id,
            reconciliation_partition_id: spec.reconciliation_partition_id,
            type_configs,
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

    /// Build a Config for unit tests without reading from disk.
    pub fn new_for_testing(
        main_environment_id: String,
        environment_metadata: Arc<DashMap<String, EnvironmentMetadata>>,
        type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
    ) -> Self {
        Config {
            public_key: vec![],
            ip_address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![],
            main_environment_id,
            reconciliation_partition_id: String::from("default"),
            environment_metadata,
            type_configs,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ConfigSpec {
    public_key: Vec<u8>,
    is_full_node: bool,
    rest_api_version: usize,
    balance: u64,
    environments: Vec<String>,
    main_env_id: String,
    reconciliation_partition_id: String
}