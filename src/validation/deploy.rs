//! `DeployValidationSpec`: sentinel-side validation for `DeployContract`
//! transactions (ADR-015, Phase 6). Fails closed on any check.
//!
//! The spec is registered by action name (`"DeployContract"`) and needs the
//! `ContractEngineRegistry` (to confirm the engine is registered) and the data
//! provider (to confirm the sender nonce). It is wired into the sentinel's
//! validation registry via `ValidationSpecRegistry::register_deploy`.

use std::sync::Arc;

use super::*;
use crate::contracts::{
    validate_wasm_module, ContractEngineRegistry, DeployParams, SPEC_MAX_BYTECODE,
    WASM_MAX_MODULE_BYTES,
};
use crate::encoding::deserialize_rmp_to;

/// Sentinel-side validation for `DeployContract` txs (ADR-015).
///
/// Checks, in order (all fail-closed):
/// 1. `tx.payload` deserializes to a `DeployParams`;
/// 2. the contract `name` is 1–64 bytes;
/// 3. the `engine` is registered in the `ContractEngineRegistry`;
/// 4. the `bytecode` is within the engine's cap (`Wasm` ≤ 1 MiB, else ≤ 64 KiB)
///    and, for `Wasm`, passes the module check (`validate_wasm_module`);
/// 5. the sender nonce (`tx.sequence_number`) matches the sender's `User.nonce`
///    (replay protection for the deterministic token id);
/// 6. the composite risk does not exceed the environment's `max_risk`.
#[derive(Clone)]
pub struct DeployValidationSpec {
    engines: Arc<ContractEngineRegistry>,
    data: Arc<dyn DataProvider>,
}

impl std::fmt::Debug for DeployValidationSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeployValidationSpec").finish()
    }
}

impl DeployValidationSpec {
    pub fn new(engines: Arc<ContractEngineRegistry>, data: Arc<dyn DataProvider>) -> Self {
        DeployValidationSpec { engines, data }
    }
}

impl TransactionValidationSpec for DeployValidationSpec {
    fn validate(
        &self,
        tx: &Transaction,
        _token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        let deploy_failed = || {
            PneumaticError::Validation(vec![ValidationFailureReason::ContractDeployFailed])
        };

        // 1. Payload parses to DeployParams.
        let params: DeployParams = match deserialize_rmp_to(&tx.payload) {
            Ok(p) => p,
            Err(_) => return Err(deploy_failed()),
        };

        // 2. Name is 1-64 bytes.
        if params.name.is_empty() || params.name.len() > 64 {
            return Err(deploy_failed());
        }

        // 3. Engine is registered.
        if self.engines.get(&params.engine).is_none() {
            return Err(deploy_failed());
        }

        // 4. Bytecode size cap + Wasm module check.
        let is_wasm = params.engine == "Wasm";
        let cap = if is_wasm { WASM_MAX_MODULE_BYTES } else { SPEC_MAX_BYTECODE };
        if params.bytecode.len() > cap {
            return Err(deploy_failed());
        }
        if is_wasm && validate_wasm_module(&params.bytecode).is_err() {
            return Err(deploy_failed());
        }

        // 5. Sender nonce matches (partition == environment_id, ADR-015 QD4).
        let user = self
            .data
            .get_user(&tx.sender, &env_data.environment_id)
            .map_err(|_| deploy_failed())?;
        if user.nonce != tx.sequence_number {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::InvalidNonce,
            ]));
        }

        // 6. Risk gate.
        let risk = self.calculate_risk(tx);
        if risk.score() > env_data.max_risk {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::RiskExceedsThreshold,
            ]));
        }
        Ok(TransactionValidationResult::valid(vec![], risk))
    }

    fn calculate_risk(&self, tx: &Transaction) -> TransactionRiskFactor {
        TransactionRiskFactor {
            affected_parties: 1,
            amount: tx.amount.unwrap_or(0),
            is_contract: true,
            is_multi_party: false,
        }
    }

    fn name(&self) -> &str {
        "DeployContract"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::StubDataProvider;
    use crate::user::User;

    // Reuse the shared core helper: environment_id "test", max_risk 1.0.
    fn test_env_data() -> EnvironmentMetadata {
        crate::validation::tests::helpers::make_env_with_defaults()
    }

    const PARTITION: &str = "test";

    fn sender_user(nonce: usize) -> User {
        let mut user = User::new(vec![0x42]);
        user.nonce = nonce;
        user
    }

    fn spec_with(data: StubDataProvider) -> DeployValidationSpec {
        let engines = Arc::new(ContractEngineRegistry::new());
        engines.register_defaults();
        DeployValidationSpec::new(engines, Arc::new(data))
    }

    fn deploy_tx(params: &DeployParams, sequence: usize) -> Transaction {
        let payload = crate::encoding::serialize_to_bytes_rmp(params).expect("rmp ok");
        Transaction {
            payload,
            gas_limit: 0,
            id: "deploy-tx".into(),
            action: "DeployContract".into(),
            token_id: vec![],
            bid: None,
            sequence_number: sequence,
            sender: vec![0x42],
            receiver: vec![],
            amount: None,
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        }
    }

    fn spec_params(name: &str, engine: &str, bytecode: &[u8]) -> DeployParams {
        DeployParams {
            name: name.to_string(),
            engine: engine.to_string(),
            bytecode: bytecode.to_vec(),
            metadata: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn valid_deploy_passes() {
        let data = StubDataProvider::new()
            .with_user(vec![0x42], PARTITION.to_string(), sender_user(5));
        let spec = spec_with(data);
        let tx = deploy_tx(&spec_params("my-contract", "Spec", b"ast-bytes"), 5);
        assert!(spec.validate(&tx, &Token::new(), &test_env_data()).is_ok());
    }

    #[test]
    fn unparseable_payload_fails() {
        let data = StubDataProvider::new()
            .with_user(vec![0x42], PARTITION.to_string(), sender_user(5));
        let spec = spec_with(data);
        let mut tx = deploy_tx(&spec_params("n", "Spec", b"x"), 5);
        tx.payload = vec![0xFF, 0xFE, 0x00]; // not valid rmp for DeployParams
        assert!(spec.validate(&tx, &Token::new(), &test_env_data()).is_err());
    }

    #[test]
    fn empty_name_fails() {
        let data = StubDataProvider::new()
            .with_user(vec![0x42], PARTITION.to_string(), sender_user(5));
        let spec = spec_with(data);
        let tx = deploy_tx(&spec_params("", "Spec", b"x"), 5);
        assert!(spec.validate(&tx, &Token::new(), &test_env_data()).is_err());
    }

    #[test]
    fn name_over_64_bytes_fails() {
        let data = StubDataProvider::new()
            .with_user(vec![0x42], PARTITION.to_string(), sender_user(5));
        let spec = spec_with(data);
        let long_name = "x".repeat(65);
        let tx = deploy_tx(&spec_params(&long_name, "Spec", b"x"), 5);
        assert!(spec.validate(&tx, &Token::new(), &test_env_data()).is_err());
    }

    #[test]
    fn unknown_engine_fails() {
        let data = StubDataProvider::new()
            .with_user(vec![0x42], PARTITION.to_string(), sender_user(5));
        let spec = spec_with(data);
        let tx = deploy_tx(&spec_params("n", "EvilEngine", b"x"), 5);
        assert!(spec.validate(&tx, &Token::new(), &test_env_data()).is_err());
    }

    #[test]
    fn spec_bytecode_over_cap_fails() {
        let data = StubDataProvider::new()
            .with_user(vec![0x42], PARTITION.to_string(), sender_user(5));
        let spec = spec_with(data);
        let big = vec![0u8; SPEC_MAX_BYTECODE + 1];
        let tx = deploy_tx(&spec_params("n", "Spec", &big), 5);
        assert!(spec.validate(&tx, &Token::new(), &test_env_data()).is_err());
    }

    #[test]
    fn wasm_invalid_module_fails() {
        let data = StubDataProvider::new()
            .with_user(vec![0x42], PARTITION.to_string(), sender_user(5));
        let spec = spec_with(data);
        // Not a valid Wasm module (fails Module::new).
        let tx = deploy_tx(&spec_params("n", "Wasm", b"not-wasm-bytes"), 5);
        assert!(spec.validate(&tx, &Token::new(), &test_env_data()).is_err());
    }

    #[test]
    fn nonce_mismatch_fails() {
        let data = StubDataProvider::new()
            .with_user(vec![0x42], PARTITION.to_string(), sender_user(5));
        let spec = spec_with(data);
        // sequence_number 7 != user.nonce 5.
        let tx = deploy_tx(&spec_params("n", "Spec", b"x"), 7);
        assert!(spec.validate(&tx, &Token::new(), &test_env_data()).is_err());
    }

    #[test]
    fn missing_sender_user_fails() {
        // No user registered for the sender in this partition.
        let data = StubDataProvider::new();
        let spec = spec_with(data);
        let tx = deploy_tx(&spec_params("n", "Spec", b"x"), 5);
        assert!(spec.validate(&tx, &Token::new(), &test_env_data()).is_err());
    }

    #[test]
    fn name_is_deploy_contract() {
        let data = StubDataProvider::new();
        let spec = spec_with(data);
        assert_eq!(spec.name(), "DeployContract");
    }
}
