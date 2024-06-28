use std::sync::Arc;
use serde::{Deserialize, Serialize};
use crate::crypto::AsymCryptoProviderType;
use crate::data::{DataProvider, DataProviderType};

//#[derive(Serialize, Deserialize)]
pub struct EnvironmentMetadata {
    pub environment_id: String,
    pub environment_name: String,
    pub token_partition_id: String,
    pub contract_partition_id: Option<String>,
    pub proxy_auth_partition_id: Option<String>,
    pub partitions: Vec<EnvironmentPartition>,
    pub quorum_percentage: f32,
    pub override_quorum_percentage: f32,
    pub data_provider: Arc<DataProvider>,
    pub asym_crypto_provider_type: AsymCryptoProviderType
    // TODO: have to finish this
}

impl EnvironmentMetadata {
    pub fn load_from_spec(spec: EnvironmentMetadataSpec) -> EnvironmentMetadata {
        let data_provider = Arc::new(DataProvider::from_environment_spec(&spec));

        let mut token_option = None;
        let mut contract_partition = None;
        let mut proxy_partition = None;
        for partition in &spec.partitions {
            match partition.partition_type {
                EnvironmentPartitionType::Token => token_option = Some(partition.id.clone()),
                EnvironmentPartitionType::Contract => contract_partition = Some(partition.id.clone()),
                EnvironmentPartitionType::ProxyAuth => proxy_partition = Some(partition.id.clone()),
                EnvironmentPartitionType::Other => ()
            }
        }

        let token_partition_id = token_option
            .expect(&format!("Environment with name \"{0}\" should have a token partition",
                             spec.environment_name));

        EnvironmentMetadata {
            environment_id: spec.environment_id,
            environment_name: spec.environment_name,
            token_partition_id,
            contract_partition_id: contract_partition,
            proxy_auth_partition_id: proxy_partition,
            partitions: spec.partitions,
            quorum_percentage: spec.quorum_percentage,
            override_quorum_percentage: spec.override_quorum_percentage,
            data_provider,
            asym_crypto_provider_type: spec.asym_crypto_provider
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct EnvironmentMetadataSpec {
    environment_id: String,
    environment_name: String,
    pub partitions: Vec<EnvironmentPartition>,
    pub asym_crypto_provider: AsymCryptoProviderType,
    sym_crypto_provider: String,
    serialization_provider: String,
    quorum_percentage: f32,
    override_quorum_percentage: f32,
    allowed_token_types: Vec<String>,
    trans_validation_specs: Vec<String>,
    block_validation_specs: Vec<String>
}

#[derive(Serialize, Deserialize, PartialEq)]
pub enum EnvironmentPartitionType {
    Token,
    Contract,
    ProxyAuth,
    Other
}

#[derive(Serialize, Deserialize)]
pub struct EnvironmentPartition {
    pub id: String,
    pub partition_type: EnvironmentPartitionType,
    pub data_provider: DataProviderType
}