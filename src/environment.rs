use std::sync::{Arc, RwLock};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::crypto;
use crate::crypto::{AsymCryptoProvider, AsymCryptoProviderType};
use crate::logging::{FileLogger, Logger};
use crate::tokens::BlockValidator;
use crate::validation::ValidationSpecRegistry;

#[derive(Clone)]
pub struct EnvironmentMetadata {
    pub environment_id: String,
    pub environment_name: String,
    pub token_partition_id: String,
    pub contract_partition_id: Option<String>,
    pub proxy_auth_partition_id: Option<String>,
    pub slush_partition_id: String,
    pub partitions: Vec<EnvironmentPartition>,
    pub quorum_percentage: f32,
    pub override_quorum_percentage: f32,
    /// Maximum risk score (0.0–1.0) allowed for transactions.
    /// Transactions exceeding this threshold are rejected by the Sentinel.
    pub max_risk: f32,
    pub asym_crypto_provider: Arc<RwLock<dyn AsymCryptoProvider>>,
    /// Block validators keyed by spec name (for per-token block validation).
    pub block_validators: Arc<DashMap<String, Box<dyn BlockValidator>>>,
    /// Transaction validation specs — action-based specs registered by name.
    pub transaction_validation_specs: Arc<ValidationSpecRegistry>,
    pub logger: Arc<dyn Logger>,
}

impl EnvironmentMetadata {
    pub fn load_from_spec(spec: EnvironmentMetadataSpec) -> EnvironmentMetadata {
        let mut token_option = None;
        let mut contract_partition = None;
        let mut proxy_partition = None;
        let mut slush_partition = None;
        for partition in &spec.partitions {
            match partition.partition_type {
                EnvironmentPartitionType::Token => token_option = Some(partition.id.clone()),
                EnvironmentPartitionType::Contract => contract_partition = Some(partition.id.clone()),
                EnvironmentPartitionType::ProxyAuth => proxy_partition = Some(partition.id.clone()),
                EnvironmentPartitionType::Slush => slush_partition = Some(partition.id.clone()),
                EnvironmentPartitionType::Other => (),
            }
        }

        let token_partition_id = token_option
            .expect(&format!(
                "Environment with name \"{0}\" should have a token partition",
                spec.environment_name
            ));

        let slush_partition_id = slush_partition
            .expect(&format!(
                "Environment with name \"{0}\" should have a slush partition",
                spec.environment_name
            ));

        let asym_crypto_provider = crypto::get_asym_provider(&spec.asym_crypto_provider);
        let logger: Arc<dyn Logger> = Arc::new(FileLogger::new(spec.log_file.clone()));

        let mut specs = ValidationSpecRegistry::new();
        specs.register_defaults();

        EnvironmentMetadata {
            environment_id: spec.environment_id,
            environment_name: spec.environment_name,
            token_partition_id,
            contract_partition_id: contract_partition,
            proxy_auth_partition_id: proxy_partition,
            slush_partition_id,
            partitions: spec.partitions,
            quorum_percentage: spec.quorum_percentage,
            override_quorum_percentage: spec.override_quorum_percentage,
            max_risk: spec.max_risk,
            asym_crypto_provider,
            block_validators: Arc::new(DashMap::new()),
            transaction_validation_specs: Arc::new(specs),
            logger,
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
    max_risk: f32,
    allowed_token_types: Vec<String>,
    trans_validation_specs: Vec<String>,
    block_validation_specs: Vec<String>,
    log_file: String,
}

#[derive(Serialize, Deserialize, PartialEq, Clone)]
pub enum EnvironmentPartitionType {
    Token,
    Contract,
    ProxyAuth,
    Slush,
    Other,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct EnvironmentPartition {
    pub id: String,
    pub partition_type: EnvironmentPartitionType,
}
