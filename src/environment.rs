use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::crypto;
use crate::crypto::{AsymCryptoProvider, AsymCryptoProviderType};
use crate::logging::{FileLogger, Logger};
use crate::tokens::BlockValidator;
use crate::validation::{BlockValidatorSpecRegistry, ValidationSpecRegistry};

/// Gas cost model: pricing for actions and protocol-level policy.
#[derive(Clone, Serialize, Deserialize)]
pub struct CostModel {
    /// Minimum gas cost per action
    pub base_cost: u64,
    /// Protocol-level minimum stake (must also meet Config::get_min_type_stake)
    pub global_min_stake: u64,
    /// Admin wallet public key for tax collection
    pub admin_public_key: Vec<u8>,
    /// Admin tax percentage (0.0–1.0, e.g. 0.02 = 2%)
    pub admin_tax_percentage: f64,
    /// Per-action amount multiplier for gas calculation.
    /// gas_used = base_cost + (transaction_amount × multiplier_for_action).
    /// Default multipliers: {"Process": 1.0, "Preload": 2.0, "Sign": 1.5}.
    #[serde(default = "CostModel::default_amount_multiplier")]
    pub amount_multiplier: HashMap<String, f64>,
}

impl CostModel {
    fn default_amount_multiplier() -> HashMap<String, f64> {
        let mut map = HashMap::new();
        map.insert("Process".to_string(), 1.0);
        map.insert("Preload".to_string(), 2.0);
        map.insert("Sign".to_string(), 1.5);
        map
    }
}

impl Default for CostModel {
    fn default() -> Self {
        CostModel {
            base_cost: 1,
            global_min_stake: 10,
            admin_public_key: vec![],
            admin_tax_percentage: 0.0,
            amount_multiplier: Self::default_amount_multiplier(),
        }
    }
}

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
    /// Gas cost model for action pricing and protocol policy
    pub cost_model: CostModel,
    pub asym_crypto_provider: Arc<RwLock<dyn AsymCryptoProvider>>,
    /// Block validators keyed by spec name (for per-token block validation).
    pub block_validators: Arc<DashMap<String, Box<dyn BlockValidator>>>,
    /// Transaction validation specs — action-based specs registered by name.
    pub transaction_validation_specs: Arc<ValidationSpecRegistry>,
    /// Block validator specs — used by Committers and Archivers.
    pub block_validator_specs: Arc<BlockValidatorSpecRegistry>,
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

        let mut block_specs = BlockValidatorSpecRegistry::new();
        block_specs.register_defaults();

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
            cost_model: spec.cost_model,
            asym_crypto_provider,
            block_validators: Arc::new(DashMap::new()),
            transaction_validation_specs: Arc::new(specs),
            block_validator_specs: Arc::new(block_specs),
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
    #[serde(default)]
    pub cost_model: CostModel,
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
