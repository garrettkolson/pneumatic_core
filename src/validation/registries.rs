//! Spec registries: `ValidationSpecRegistry` (token-level specs by id, the
//! Box dyn blanket impls) and `BlockValidatorSpecRegistry`.

use super::*;

// ---------------------------------------------------------------------------
// ValidationSpecRegistry — stores and looks up specs by action name
// ---------------------------------------------------------------------------

/// Registry of TransactionValidationSpec instances, keyed by spec name.
/// Used by Sentinels to look up the correct validation spec for each
/// transaction action.
#[derive(Default)]
pub struct ValidationSpecRegistry {
    specs: HashMap<String, Arc<dyn TransactionValidationSpec>>,
}

impl ValidationSpecRegistry {
    pub fn new() -> Self {
        ValidationSpecRegistry {
            specs: HashMap::new(),
        }
    }

    /// Register a validation spec under a name.
    pub fn register(&mut self, spec: Box<dyn TransactionValidationSpec>) {
        let name = spec.name().to_string();
        let spec: Arc<dyn TransactionValidationSpec> = Arc::from(spec);
        self.specs.insert(name, spec);
    }

    /// Look up a spec by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn TransactionValidationSpec>> {
        self.specs.get(name)
    }

    /// Register default specs (SelfSigned and Executed).
    pub fn register_defaults(&mut self) {
        self.register(Box::new(SelfSignedBlockValidatorSpec::new()));
        self.register(Box::new(ExecutedBlockValidatorSpec::new(0)));
    }

    /// Register the shielded validation spec (`"Shielded"`).
    ///
    /// Distinct from `register_defaults`: opt-in only, so a non-shielded
    /// deployment does not construct the Halo2 verifying key. Registered under
    /// the name `ShieldedValidationSpec::NAME`; fail-closed for a `"Shielded"`
    /// tx that reaches an unprepared spec (see the trait default impl).
    pub fn register_shielded(&mut self) {
        self.register(Box::new(ShieldedValidationSpec::new()));
    }
}

// Blanket impl: Box<dyn TransactionValidationSpec> delegates to the inner trait object.
// This allows Arc::new(Box<dyn Spec>) to be used where Arc<dyn Spec> is expected.
impl TransactionValidationSpec for Box<dyn TransactionValidationSpec> {
    fn validate(
        &self,
        tx: &Transaction,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        (**self).validate(tx, token, env_data)
    }

    fn calculate_risk(&self, tx: &Transaction) -> TransactionRiskFactor {
        (**self).calculate_risk(tx)
    }

    fn name(&self) -> &str {
        (**self).name()
    }

    /// Delegate shielded validation so a `ShieldedValidationSpec` held behind a
    /// `Box<dyn TransactionValidationSpec>` still validates shielded txs (the
    /// trait default impl below would otherwise fail it closed).
    fn validate_shielded(
        &self,
        tx: &ShieldedTransaction,
        env_data: &EnvironmentMetadata,
        deps: &ShieldedValidationDeps,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        (**self).validate_shielded(tx, env_data, deps)
    }
}

// ---------------------------------------------------------------------------
// BlockValidatorSpecRegistry — stores and looks up BlockValidatorSpec instances
// ---------------------------------------------------------------------------

/// Registry of BlockValidatorSpec instances, keyed by spec name.
/// Used by Committers and Archivers to look up the correct block validation
/// spec for each token's blocks.
#[derive(Default)]
pub struct BlockValidatorSpecRegistry {
    specs: HashMap<String, Arc<dyn BlockValidatorSpec>>,
}

impl BlockValidatorSpecRegistry {
    pub fn new() -> Self {
        BlockValidatorSpecRegistry {
            specs: HashMap::new(),
        }
    }

    /// Register a block validator spec under a given name.
    pub fn register(&mut self, name: &str, spec: Box<dyn BlockValidatorSpec>) {
        let spec: Arc<dyn BlockValidatorSpec> = Arc::from(spec);
        self.specs.insert(name.to_string(), spec);
    }

    /// Look up a spec by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn BlockValidatorSpec>> {
        self.specs.get(name)
    }

    /// Register default specs (SelfSigned and Executed).
    pub fn register_defaults(&mut self) {
        self.register("SelfSigned", Box::new(SelfSignedBlockValidatorSpec::new()));
        self.register("Executed", Box::new(ExecutedBlockValidatorSpec::new(0)));
    }
}

// Blanket impl: Box<dyn BlockValidatorSpec> delegates to the inner trait object.
impl BlockValidatorSpec for Box<dyn BlockValidatorSpec> {
    fn validate(
        &self,
        block: &crate::blocks::Block,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<BlockValidationResult, PneumaticError> {
        (**self).validate(block, token, env_data)
    }
}
