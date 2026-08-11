use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::crypto;
use crate::crypto::{AsymCryptoProvider, AsymCryptoProviderType};
use crate::logging::{FileLogger, Logger};
use crate::tokens::BlockValidator;
use crate::validation::{BlockValidatorSpecRegistry, ValidationSpecRegistry,
    SelfSignedBlockValidatorSpec, ExecutedBlockValidatorSpec};

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

    /// Fixed-point scale for integer gas computation.
    ///
    /// Converting the f64 multiplier to an integer at this scale ensures
    /// that all per-transaction gas arithmetic is bitwise-identical across
    /// CPU architectures (no FPU precision differences). 10_000 gives
    /// four decimal places of precision — sufficient for multipliers such
    /// as 1.0, 1.5, 2.0 and 2.5.
    const GAS_SCALE: u64 = 10_000;

    /// Convert an f64 amount multiplier to an integer fixed-point value.
    ///
    /// e.g. `1.5 → 15000`, `2.0 → 20000`, `0.5 → 5000`.
    fn multiplier_to_fixed(multiplier: f64) -> u64 {
        (multiplier * Self::GAS_SCALE as f64).round() as u64
    }

    /// Compute the gas attributable to a transaction amount using integer
    /// fixed-point arithmetic.
    ///
    /// `gas_from_amount = (amount × multiplier_fixed) / GAS_SCALE`
    ///
    /// Uses `saturating_mul` so that no malformed or extreme amount can
    /// cause an arithmetic panic.
    fn gas_from_amount(amount: u64, multiplier: u64) -> u64 {
        amount.saturating_mul(multiplier) / Self::GAS_SCALE
    }

    /// Compute total gas for a transaction.
    ///
    /// `gas_used = base_cost + gas_from_amount(amount, multiplier)`
    ///
    /// All arithmetic is integer-based, guaranteeing bitwise-identical
    /// results across every CPU architecture.
    pub fn compute_gas(&self, amount: u64, multiplier: f64) -> u64 {
        let multiplier_fixed = Self::multiplier_to_fixed(multiplier);
        let gas_from_amount = Self::gas_from_amount(amount, multiplier_fixed);
        self.base_cost.saturating_add(gas_from_amount)
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
    /// Allowed token types for this environment (from spec).
    pub allowed_token_types: Vec<String>,
    /// Symmetric crypto provider identifier (from spec).
    /// AES-256-GCM is used directly via Ed25519Provider — no pluggable layer.
    pub sym_crypto_provider: String,
    /// Serialization provider identifier (from spec).
    /// rmp-serde (MsgPack) is the wire format; serde_json for config files.
    pub serialization_provider: String,
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

        // Store crypto/serialization provider identifiers for diagnostics.
        // Symmetric crypto is AES-256-GCM (baked into Ed25519Provider);
        // serialization is rmp-serde (MsgPack) for wire format.
        let sym_crypto_provider = spec.sym_crypto_provider.clone();
        let serialization_provider = spec.serialization_provider.clone();

        let mut specs = ValidationSpecRegistry::new();
        specs.register_defaults();

        // Wire trans_validation_specs from JSON into the transaction validation registry.
        // Names not found in defaults are silently skipped (graceful degradation).
        for name in &spec.trans_validation_specs {
            match name.as_str() {
                "SelfSigned" => specs.register(Box::new(SelfSignedBlockValidatorSpec::new())),
                "Executed" => specs.register(Box::new(ExecutedBlockValidatorSpec::new(0))),
                _ => {} // silently skip unknown spec names
            }
        }

        let mut block_specs = BlockValidatorSpecRegistry::new();
        block_specs.register_defaults();

        // Wire block_validation_specs from JSON into the block validator registry.
        for name in &spec.block_validation_specs {
            match name.as_str() {
                "SelfSigned" => block_specs.register("SelfSigned", Box::new(SelfSignedBlockValidatorSpec::new())),
                "Executed" => block_specs.register("Executed", Box::new(ExecutedBlockValidatorSpec::new(0))),
                _ => {} // silently skip unknown spec names
            }
        }

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
            allowed_token_types: spec.allowed_token_types,
            sym_crypto_provider,
            serialization_provider,
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
