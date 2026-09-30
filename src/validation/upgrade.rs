//! `UpgradeValidationSpec`: sentinel-side validation for `UpgradeContract`
//! transactions (ADR-017, Phase 8). Fails closed on any check.
//!
//! The spec is registered by action name (`"UpgradeContract"`) and needs the
//! `ContractEngineRegistry` (bytecode cap + Wasm module check + scanner) and the
//! environment's crypto provider (the M-of-N quorum check). It is wired into the
//! sentinel's validation registry via `ValidationSpecRegistry::register_upgrade`.
//!
//! The **timelock is NOT checked here** — it is enforced at apply time by the
//! committer (ADR-017 QD3). The sentinel admits the *proposal*; the committer gates
//! the *apply* against the committed block's `epoch_number`.

use std::sync::Arc;

use super::*;
use crate::contracts::{
    scan_contract, validate_wasm_module, verify_quorum, ContractEngineRegistry,
    SPEC_MAX_BYTECODE, UpgradeParams, WASM_MAX_MODULE_BYTES,
};
use crate::crypto::BasicHashProvider;
use crate::encoding::deserialize_rmp_to;
use crate::tokens::SmartContract;

/// Sentinel-side validation for `UpgradeContract` txs (ADR-017).
///
/// Checks, in order (all fail-closed):
/// 1. `tx.payload` deserializes to an `UpgradeParams`;
/// 2. the target token is a contract (`token_type == "contract"`);
/// 3. the target is upgradeable (current `threshold > 0`);
/// 4. the M-of-N quorum is met (≥ `threshold` distinct *current* owners sign the
///    canonical upgrade digest);
/// 5. the new `bytecode` is within the engine's cap (`Wasm` ≤ 1 MiB, else ≤ 64 KiB)
///    and, for `Wasm`, passes the module check + the deploy-time scanner (no
///    `Reject`-severity finding);
/// 6. the composite risk does not exceed the environment's `max_risk`.
#[derive(Clone)]
pub struct UpgradeValidationSpec {
    engines: Arc<ContractEngineRegistry>,
}

impl std::fmt::Debug for UpgradeValidationSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeValidationSpec").finish()
    }
}

impl UpgradeValidationSpec {
    pub fn new(engines: Arc<ContractEngineRegistry>) -> Self {
        UpgradeValidationSpec { engines }
    }
}

impl TransactionValidationSpec for UpgradeValidationSpec {
    fn validate(
        &self,
        tx: &Transaction,
        token: &Token,
        env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        let failed = || {
            PneumaticError::Validation(vec![ValidationFailureReason::ContractUpgradeFailed])
        };

        // 1. Payload parses to UpgradeParams.
        let params: UpgradeParams = match deserialize_rmp_to(&tx.payload) {
            Ok(p) => p,
            Err(_) => return Err(failed()),
        };

        // 2. Target is a contract.
        if token.metadata.get("token_type").map(|s| s.as_str()) != Some("contract") {
            return Err(failed());
        }

        // 3. Read the current owner registry + engine; require upgradeable.
        let current: SmartContract = match token.get_asset() {
            Some(c) => c,
            None => return Err(failed()),
        };
        if current.threshold == 0 {
            return Err(failed());
        }

        // 4. M-of-N quorum against the CURRENT owners (QD2).
        let hash = BasicHashProvider::new();
        let crypto = env_data.asym_crypto_provider.read().unwrap();
        let quorum = match verify_quorum(
            &tx.token_id,
            &params.new_bytecode,
            &params.new_owners,
            params.new_threshold,
            params.proposal_epoch,
            &params.owner_signatures,
            &current.owners,
            current.threshold,
            &*crypto,
            &hash,
        ) {
            Ok(q) => q,
            Err(_) => return Err(failed()),
        };
        if !quorum {
            return Err(failed());
        }

        // 5. Bytecode size cap + Wasm module check + scanner.
        let engine = token
            .metadata
            .get("contract_engine")
            .cloned()
            .unwrap_or_default();
        let is_wasm = engine == "Wasm";
        let cap = if is_wasm { WASM_MAX_MODULE_BYTES } else { SPEC_MAX_BYTECODE };
        if params.new_bytecode.len() > cap {
            return Err(failed());
        }
        if is_wasm && validate_wasm_module(&params.new_bytecode).is_err() {
            return Err(failed());
        }
        let scan_findings =
            scan_contract(&self.engines, &engine, &params.new_bytecode);
        for f in &scan_findings {
            if f.severity == crate::contracts::Severity::Reject {
                env_data
                    .logger
                    .log(format!("[upgrade-scan] REJECT {:?}: {}", f.code, f.detail));
                return Err(PneumaticError::Validation(vec![
                    ValidationFailureReason::ContractScanFailed,
                ]));
            }
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
        "UpgradeContract"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{AsymCryptoProvider, Ed25519Provider};

    /// A contract token with the given owner set + threshold.
    fn contract_token(id: &[u8], owners: Vec<Vec<u8>>, threshold: u32) -> Token {
        let mut token = Token::new();
        token.id = id.to_vec();
        token
            .metadata
            .insert("token_type".to_string(), "contract".to_string());
        token
            .metadata
            .insert("contract_engine".to_string(), "Spec".to_string());
        let contract = SmartContract {
            name: "c".to_string(),
            bytecode: b"old".to_vec(),
            version: "1".to_string(),
            storage: Default::default(),
            owners,
            threshold,
        };
        token.set_asset(&contract).unwrap();
        token
    }

    fn test_env() -> EnvironmentMetadata {
        crate::validation::tests::helpers::make_env_with_defaults()
    }

    /// A registry with the default engines (`Transfer` + `Spec`) so the deploy-time
    /// scanner can run its `Spec` canary on new bytecode.
    fn default_engines() -> Arc<ContractEngineRegistry> {
        let reg = ContractEngineRegistry::new();
        reg.register_defaults();
        Arc::new(reg)
    }

    /// Build an `UpgradeContract` tx with the given payload.
    fn upgrade_tx(payload: &UpgradeParams, token_id: &[u8]) -> Transaction {
        let payload = crate::encoding::serialize_to_bytes_rmp(payload).unwrap();
        Transaction {
            payload,
            gas_limit: 0,
            id: "upgrade-tx".into(),
            action: "UpgradeContract".into(),
            token_id: token_id.to_vec(),
            bid: None,
            sequence_number: 7,
            sender: vec![0x42; 32],
            receiver: vec![],
            amount: None,
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
            result_data: vec![],
        }
    }

    #[test]
    fn name_is_upgrade_contract() {
        let spec = UpgradeValidationSpec::new(Arc::new(ContractEngineRegistry::new()));
        assert_eq!(spec.name(), "UpgradeContract");
    }

    #[test]
    fn non_contract_target_fails() {
        let engines = Arc::new(ContractEngineRegistry::new());
        let spec = UpgradeValidationSpec::new(engines);
        let token = Token::new(); // not a contract
        let params = UpgradeParams {
            new_bytecode: b"new".to_vec(),
            new_owners: vec![],
            new_threshold: 0,
            proposal_epoch: 7,
            owner_signatures: vec![],
        };
        let tx = upgrade_tx(&params, b"tok");
        let err = spec.validate(&tx, &token, &test_env()).unwrap_err();
        match err {
            PneumaticError::Validation(reasons) => {
                assert!(reasons.contains(&ValidationFailureReason::ContractUpgradeFailed))
            }
            other => panic!("expected ContractUpgradeFailed, got {other:?}"),
        }
    }

    #[test]
    fn immutable_contract_threshold_zero_fails() {
        let engines = Arc::new(ContractEngineRegistry::new());
        let spec = UpgradeValidationSpec::new(engines);
        let token = contract_token(b"tok", vec![], 0);
        let params = UpgradeParams {
            new_bytecode: b"new".to_vec(),
            new_owners: vec![],
            new_threshold: 0,
            proposal_epoch: 7,
            owner_signatures: vec![],
        };
        let tx = upgrade_tx(&params, b"tok");
        assert!(spec.validate(&tx, &token, &test_env()).is_err());
    }

    #[test]
    fn quorum_shortfall_fails_and_full_quorum_passes() {
        let engines = default_engines();
        let spec = UpgradeValidationSpec::new(engines);
        let hash = BasicHashProvider::new();

        // Two owners, threshold 2.
        let o1 = Ed25519Provider::generate();
        let o2 = Ed25519Provider::generate();
        let pk1 = o1.public_key().unwrap();
        let pk2 = o2.public_key().unwrap();
        let token = contract_token(b"tok", vec![pk1.clone(), pk2.clone()], 2);

        // A minimal valid Spec program so the deploy-time scanner (which the
        // upgrade spec also runs) does not reject the new bytecode.
        let new_bytecode = crate::encoding::serialize_to_bytes_rmp(
            &crate::contracts::InstructionProgram {
                version: 1,
                ops: vec![
                    crate::contracts::Op::LoadConst(1),
                    crate::contracts::Op::Emit,
                    crate::contracts::Op::Halt,
                ],
            },
        )
        .unwrap();
        let new_owners = vec![];
        let new_threshold = 1u32;
        let proposal_epoch = 7u64;
        let digest = crate::contracts::upgrade_digest(
            b"tok",
            &new_bytecode,
            &new_owners,
            new_threshold,
            proposal_epoch,
            &hash,
        );

        // M-1 (only o1 signs) → fails.
        let params_m1 = UpgradeParams {
            new_bytecode: new_bytecode.clone(),
            new_owners: new_owners.clone(),
            new_threshold,
            proposal_epoch,
            owner_signatures: vec![o1.sign_data(&digest).unwrap()],
        };
        let tx_m1 = upgrade_tx(&params_m1, b"tok");
        assert!(
            spec.validate(&tx_m1, &token, &test_env()).is_err(),
            "M-1 signatures must fail"
        );

        // M (o1 + o2 sign) → passes.
        let params_m = UpgradeParams {
            new_bytecode,
            new_owners,
            new_threshold,
            proposal_epoch,
            owner_signatures: vec![
                o1.sign_data(&digest).unwrap(),
                o2.sign_data(&digest).unwrap(),
            ],
        };
        let tx_m = upgrade_tx(&params_m, b"tok");
        assert!(
            spec.validate(&tx_m, &token, &test_env()).is_ok(),
            "M signatures must pass"
        );
    }
}
