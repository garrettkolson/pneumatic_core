use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use serde::{Deserialize, Serialize};

use crate::crypto;
use crate::crypto::{AsymCryptoProvider, AsymCryptoProviderType};
use crate::errors::PneumaticError;
use crate::logging::{FileLogger, Logger};
use crate::node::NodeRegistryType;
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
    /// Per-node-type minimum stake overrides for this environment. Each entry
    /// replaces the protocol-level default (`Config::get_min_type_stake`) for
    /// its type; an empty map (the default when the field is omitted) leaves
    /// every type at the uniform default. This is an env-spec *config* field,
    /// not an RNS control-plane wire message — its `#[serde(default)]` makes
    /// env specs without it degrade to an empty map (fully backward/forward
    /// compatible; see AUDIT Phase 4.4 wire-compat note).
    #[serde(default)]
    pub per_type_min_stake: HashMap<NodeRegistryType, u64>,
    /// Fraction of a double-signed proposer's stake to slash on a resolved
    /// same-proposer conflict (0.0–1.0, e.g. 1.0 = full stake). This is an
    /// env-spec *config* field, not an RNS control-plane wire message — its
    /// `#[serde(default)]` makes env specs without it degrade to a full-stake
    /// slash (fully backward/forward compatible, like `per_type_min_stake`).
    #[serde(default = "CostModel::default_slash_fraction")]
    pub slash_fraction: f64,
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

    /// The protocol-level global minimum stake used when an environment has
    /// no configured `CostModel.global_min_stake` (e.g. tests that construct
    /// a `Config` without a populated environment registry). Must match
    /// `CostModel::default().global_min_stake` so the fallback never widens or
    /// tightens the floor.
    pub fn default_global_min_stake() -> u64 {
        10
    }

    /// Default slash fraction — full stake. A double-signed proposer loses
    /// their entire current stake unless an environment overrides
    /// `CostModel.slash_fraction` to a smaller value.
    pub fn default_slash_fraction() -> f64 {
        1.0
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
            per_type_min_stake: HashMap::new(),
            slash_fraction: 1.0,
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
    /// Number of executor shards. Default 1 = no sharding.
    pub shard_count: u32,
    /// Quorum percentage within a shard (e.g. 67.0 = 2/3).
    /// Used by the signature collector when accumulating stake.
    pub shard_quorum_percentage: f32,
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
            transaction_validation_specs: Arc::new(specs),
            block_validator_specs: Arc::new(block_specs),
            allowed_token_types: spec.allowed_token_types,
            sym_crypto_provider,
            serialization_provider,
            logger,
            shard_count: spec.shard_count,
            shard_quorum_percentage: spec.shard_quorum_percentage,
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
    /// Number of executor shards. Default 1 = no sharding.
    #[serde(default = "default_shard_count")]
    pub shard_count: u32,
    /// Quorum percentage within a shard. Default 67.0 (2/3).
    #[serde(default = "default_shard_quorum_percentage")]
    pub shard_quorum_percentage: f32,
}

fn default_shard_count() -> u32 { 1 }
fn default_shard_quorum_percentage() -> f32 { 67.0 }

impl EnvironmentMetadataSpec {
    /// Validate the protocol-relevant numeric fields of an environment spec
    /// *before* it is loaded, so a spec that would neuter the protocol is
    /// rejected at boot rather than silently accepted.
    ///
    /// Ranges enforced (this is the fail-closed config gate for audit Phase
    /// 5.7 / H6):
    /// - `quorum_percentage`, `shard_quorum_percentage` ∈ (0, 100]: a 0%
    ///   finalizer quorum would be satisfied by zero signatures; anything
    ///   above 100% is unsatisfiable and deadlocks finalization.
    /// - `override_quorum_percentage` ∈ [0, 100]: 0 is the documented
    ///   "disabled" sentinel; above 100% is unsatisfiable.
    /// - `max_risk`, `cost_model.admin_tax_percentage` ∈ [0, 1].
    /// - every `cost_model.amount_multiplier` value is finite and ≥ 0: a
    ///   negative/non-finite multiplier would be cast to a bogus `u64` and
    ///   make that action effectively free.
    /// - `shard_count` ≥ 1.
    ///
    /// All violations are collected into a single error so a bad spec reports
    /// every problem at once.
    pub fn validate(&self) -> Result<(), PneumaticError> {
        let mut violations: Vec<String> = Vec::new();

        let ok_quorum = |v: f32| v.is_finite() && v > 0.0 && v <= 100.0;
        let ok_percent = |v: f32| v.is_finite() && v >= 0.0 && v <= 100.0;
        // `max_risk` is compared against the 0.0-1.0 risk score, so it is a
        // ratio, not a percentage.
        let ok_ratio = |v: f32| v.is_finite() && v >= 0.0 && v <= 1.0;

        if !ok_quorum(self.quorum_percentage) {
            violations.push(format!(
                "quorum_percentage {} is outside (0, 100]",
                self.quorum_percentage
            ));
        }
        if !ok_quorum(self.shard_quorum_percentage) {
            violations.push(format!(
                "shard_quorum_percentage {} is outside (0, 100]",
                self.shard_quorum_percentage
            ));
        }
        if !ok_percent(self.override_quorum_percentage) {
            violations.push(format!(
                "override_quorum_percentage {} is outside [0, 100]",
                self.override_quorum_percentage
            ));
        }
        if !ok_ratio(self.max_risk) {
            violations.push(format!(
                "max_risk {} is outside [0, 1]",
                self.max_risk
            ));
        }
        if self.shard_count < 1 {
            violations.push(format!("shard_count {} must be >= 1", self.shard_count));
        }

        // Economic policy lives on the cost model.
        if !self.cost_model.admin_tax_percentage.is_finite()
            || self.cost_model.admin_tax_percentage < 0.0
            || self.cost_model.admin_tax_percentage > 1.0
        {
            violations.push(format!(
                "admin_tax_percentage {} is outside [0, 1]",
                self.cost_model.admin_tax_percentage
            ));
        }
        for (action, mult) in &self.cost_model.amount_multiplier {
            if !mult.is_finite() || *mult < 0.0 {
                violations.push(format!(
                    "amount_multiplier[{action}] {} must be finite and >= 0",
                    *mult
                ));
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(PneumaticError::Encoding(format!(
                "invalid environment spec: {}",
                violations.join("; ")
            )))
        }
    }
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

// ---------------------------------------------------------------------------
// Tests — EnvironmentMetadataSpec::validate
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // A spec that satisfies every range check.
    const VALID_BASE: &str = r#"{
        "environment_id":"test",
        "environment_name":"test",
        "partitions":[
            {"id":"token-part","partition_type":"Token"},
            {"id":"slush-part","partition_type":"Slush"}
        ],
        "asym_crypto_provider":{"Ed25519":null},
        "sym_crypto_provider":"sym",
        "serialization_provider":"rmp",
        "quorum_percentage":67.0,
        "override_quorum_percentage":1.0,
        "max_risk":1.0,
        "cost_model":{
            "base_cost":1,
            "global_min_stake":10,
            "admin_public_key":[],
            "admin_tax_percentage":0.0,
            "amount_multiplier":{"Process":1.0,"Preload":2.0,"Sign":1.5},
            "per_type_min_stake":{},
            "slash_fraction":1.0
        },
        "allowed_token_types":[],
        "trans_validation_specs":[],
        "block_validation_specs":[],
        "log_file":"test.log",
        "shard_count":1,
        "shard_quorum_percentage":67.0
    }"#;

    fn parse(value: serde_json::Value) -> EnvironmentMetadataSpec {
        serde_json::from_value(value).expect("spec JSON must be valid")
    }

    #[test]
    fn spec_validate_accepts_valid_defaults() {
        let value: serde_json::Value = serde_json::from_str(VALID_BASE).unwrap();
        assert!(parse(value).validate().is_ok());
    }

    #[test]
    fn spec_validate_rejects_quorum_percentage_zero() {
        // quorum_percentage = 0 makes finalizer quorum always satisfied — the
        // boot-failure case (0 signatures counts as quorum).
        let mut value: serde_json::Value = serde_json::from_str(VALID_BASE).unwrap();
        value["quorum_percentage"] = json!(0.0);
        assert!(parse(value).validate().is_err());
    }

    #[test]
    fn spec_validate_rejects_quorum_percentage_over_100() {
        let mut value: serde_json::Value = serde_json::from_str(VALID_BASE).unwrap();
        value["quorum_percentage"] = json!(120.0);
        assert!(parse(value).validate().is_err());
    }

    #[test]
    fn spec_validate_rejects_shard_quorum_percentage_zero() {
        let mut value: serde_json::Value = serde_json::from_str(VALID_BASE).unwrap();
        value["shard_quorum_percentage"] = json!(0.0);
        assert!(parse(value).validate().is_err());
    }

    #[test]
    fn spec_validate_rejects_override_quorum_over_100() {
        let mut value: serde_json::Value = serde_json::from_str(VALID_BASE).unwrap();
        value["override_quorum_percentage"] = json!(150.0);
        assert!(parse(value).validate().is_err());
    }

    #[test]
    fn spec_validate_rejects_max_risk_over_one() {
        let mut value: serde_json::Value = serde_json::from_str(VALID_BASE).unwrap();
        value["max_risk"] = json!(1.5);
        assert!(parse(value).validate().is_err());
    }

    #[test]
    fn spec_validate_rejects_admin_tax_over_one() {
        let mut value: serde_json::Value = serde_json::from_str(VALID_BASE).unwrap();
        value["cost_model"]["admin_tax_percentage"] = json!(2.0);
        assert!(parse(value).validate().is_err());
    }

    #[test]
    fn spec_validate_rejects_negative_amount_multiplier() {
        // A negative multiplier is cast to a bogus u64 in
        // CostModel::multiplier_to_fixed, making that action effectively free.
        let mut value: serde_json::Value = serde_json::from_str(VALID_BASE).unwrap();
        value["cost_model"]["amount_multiplier"]["Process"] = json!(-1.0);
        assert!(parse(value).validate().is_err());
    }

    #[test]
    fn spec_validate_rejects_shard_count_zero() {
        let mut value: serde_json::Value = serde_json::from_str(VALID_BASE).unwrap();
        value["shard_count"] = json!(0);
        assert!(parse(value).validate().is_err());
    }
}
