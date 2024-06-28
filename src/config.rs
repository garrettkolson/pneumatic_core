use std::fs;
use std::io::Error;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::data::DataProviderType;
use crate::encoding;
use crate::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
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
    pub main_data_provider_type: DataProviderType,
    pub main_environment_id: String,
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

        // todo: determine node registry types for this node

        // build up environment metadata
        let environment_metadata = match Config::get_environment_metadata() {
            Ok(result) => result,
            Err(err) => return Err(NodeBootstrapError::from_io_error(err))
        };

        // todo: calculate min/max # of connections
        // todo: calculate/set minimum stake for each node type

        Ok(Config {
            public_key: vec![],
            ip_address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            rest_api_version: spec.rest_api_version,
            node_type: if spec.is_full_node { NodeType::Full } else { NodeType::Light },
            node_registry_types: vec![],
            main_data_provider_type: DataProviderType::RocksDb,
            environment_metadata,
            main_environment_id: String::from(""),
            type_configs: Arc::new(DashMap::new())
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
}

#[derive(Serialize, Deserialize)]
pub struct ConfigSpec {
    public_key: Vec<u8>,
    is_full_node: bool,
    rest_api_version: usize,
    balance: u64,
    environments: Vec<String>
}