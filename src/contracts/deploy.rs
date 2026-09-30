//! On-chain contract deployment (ADR-015, Phase 6).
//!
//! Deployment is a **protocol op**, not contract logic: a `DeployContract` tx
//! creates a contract token on-chain, engine-agnostic (both `Spec` and `Wasm`).
//! The executor stays pure (ADR-013) — it emits a canonical [`CreateTokenDelta`]
//! in `result_data`; the data service applies it at commit (idempotent).
//!
//! Consensus-critical pieces pinned here (changing them is a hard fork):
//! - the deterministic [`derive_token_id`] formula (CREATE2-style, domain-tagged);
//! - the [`CreateTokenDelta`] schema;
//! - the deployment gas formula ([`deploy_gas`]).
//!
//! Partitioning (QD4): a new token lives in its **environment** partition
//! (`partition_id == environment_id`), matching the committer's existing
//! `get_token(token_id, &env_id)` addressing.

use std::collections::HashMap;

use serde::Serialize;

use crate::crypto::HashProvider;
use crate::encoding::serialize_to_bytes_rmp;
use crate::tokens::{SmartContract, Token};

use super::ContractError;

/// Domain tag for the deterministic `token_id` (collision-resistant, versioned).
const DEPLOY_DOMAIN: &[u8] = b"PNEUMATIC/DEPLOY/v1";

/// Deployment gas: a flat base plus a per-bytecost of the bytecode.
pub const DEPLOY_GAS_BASE: u64 = 50_000;
/// Per-byte deployment gas (bounds a 1 MiB Wasm module at ~150_000 gas).
pub const DEPLOY_GAS_PER_BYTE: u64 = 10;
/// Bytecode cap for `Spec` contracts (rmp-AST, far smaller than Wasm).
pub const SPEC_MAX_BYTECODE: usize = 64 * 1024;

/// Deployment parameters carried in a `DeployContract` tx `payload`
/// (rmp-canonical). The `sender`, `nonce`, and `gas_limit` come from the tx
/// envelope and the sender's `User` state, not here.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct DeployParams {
    /// Human-readable contract name (1–64 bytes, enforced by the validation spec).
    pub name: String,
    /// Registered engine name (`"Spec"`, `"Wasm"`, ...).
    pub engine: String,
    /// Contract bytecode: a `Spec` rmp-AST or a `Wasm` module.
    pub bytecode: Vec<u8>,
    /// Extra user-supplied metadata keys (the required `token_type` /
    /// `contract_engine` / `name` keys are set by the committer, not here).
    pub metadata: HashMap<String, String>,
}

/// The canonical `CreateToken` state delta the executor emits in `result_data`.
/// The committer reconstructs the [`Token`] from it and `save_token`s it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct CreateTokenDelta {
    /// The deterministic new token id (from [`derive_token_id`]).
    pub token_id: Vec<u8>,
    /// Contract name.
    pub name: String,
    /// Registered engine name.
    pub engine: String,
    /// Contract bytecode.
    pub bytecode: Vec<u8>,
    /// Extra user-supplied metadata keys.
    pub metadata: HashMap<String, String>,
}

/// Deployment gas cost for a bytecode of `bytecode_len` bytes.
///
/// `deploy_gas = DEPLOY_GAS_BASE + DEPLOY_GAS_PER_BYTE * bytecode_len`.
pub fn deploy_gas(bytecode_len: usize) -> u64 {
    DEPLOY_GAS_BASE + DEPLOY_GAS_PER_BYTE * (bytecode_len as u64)
}

/// CREATE2-style deterministic token id, derivable before execution and
/// identical on every shard member:
///
/// ```text
/// token_id = SHA256( DEPLOY_DOMAIN ‖ rmp_canon( deployer_pubkey,
///                                                 nonce,
///                                                 SHA256(bytecode),
///                                                 name ) )
/// ```
///
/// `nonce` is the sender's `User.nonce` (per-sender): reusing a nonce yields a
/// different id at the id level, and the validation spec rejects the replay
/// before routing. `SHA256(bytecode)` (not the raw bytes) keeps the rmp payload
/// small for large Wasm modules.
pub fn derive_token_id(
    deployer_pubkey: &[u8],
    nonce: u64,
    bytecode: &[u8],
    name: &str,
    hash: &dyn HashProvider,
) -> Vec<u8> {
    let bytecode_hash = hash.hash(bytecode);
    let payload = serialize_to_bytes_rmp(&DeployIdPayload {
        deployer_pubkey: deployer_pubkey.to_vec(),
        nonce,
        bytecode_hash,
        name: name.to_string(),
    })
    .expect("rmp serialization of the id payload cannot fail");
    let mut tagged = Vec::with_capacity(DEPLOY_DOMAIN.len() + payload.len());
    tagged.extend_from_slice(DEPLOY_DOMAIN);
    tagged.extend_from_slice(&payload);
    hash.hash(&tagged)
}

/// Private, order-stable carrier for the rmp-canonical id payload. Field
/// declaration order is the canonical field order (rmp-serde preserves it).
#[derive(Serialize)]
struct DeployIdPayload {
    deployer_pubkey: Vec<u8>,
    nonce: u64,
    bytecode_hash: Vec<u8>,
    name: String,
}

/// The pure deployment op: compute the deterministic `token_id` and build the
/// canonical [`CreateTokenDelta`]. It trusts that the inputs were already
/// validated by the sentinel-side `DeployValidationSpec` (engine registered,
/// bytecode size + format, name rules, Wasm module check, nonce).
pub fn deploy_contract(
    deployer_pubkey: &[u8],
    nonce: u64,
    params: &DeployParams,
    hash: &dyn HashProvider,
) -> Result<CreateTokenDelta, ContractError> {
    if params.name.is_empty() {
        return Err(ContractError::InvalidInput("contract name is empty".into()));
    }
    if params.engine.is_empty() {
        return Err(ContractError::InvalidInput("engine name is empty".into()));
    }
    let token_id = derive_token_id(deployer_pubkey, nonce, &params.bytecode, &params.name, hash);
    Ok(CreateTokenDelta {
        token_id,
        name: params.name.clone(),
        engine: params.engine.clone(),
        bytecode: params.bytecode.clone(),
        metadata: params.metadata.clone(),
    })
}

impl CreateTokenDelta {
    /// Reconstruct the [`Token`] this delta creates, in `env_id`.
    ///
    /// The required metadata keys (`token_type`, `contract_engine`, `name`) are
    /// set here (by the committer), not from the sender-supplied `metadata`, so
    /// a sender cannot forge a non-contract token or a wrong engine. `version`
    /// defaults to `"1"`.
    pub fn to_token(&self, env_id: &str) -> Result<Token, ContractError> {
        let contract = SmartContract {
            name: self.name.clone(),
            bytecode: self.bytecode.clone(),
            version: "1".to_string(),
            storage: Default::default(),
        };
        let mut token = Token::from_asset(&contract)
            .map_err(|e| ContractError::InvalidInput(format!("from_asset: {e}")))?;
        token.id = self.token_id.clone();
        let mut metadata = HashMap::new();
        for (k, v) in &self.metadata {
            metadata.insert(k.clone(), v.clone());
        }
        metadata.insert("token_type".to_string(), "contract".to_string());
        metadata.insert("contract_engine".to_string(), self.engine.clone());
        metadata.insert("name".to_string(), self.name.clone());
        token.metadata = metadata;
        token.environment_id = env_id.to_string();
        Ok(token)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::BasicHashProvider;

    fn hash() -> BasicHashProvider {
        BasicHashProvider::new()
    }

    fn params(name: &str, engine: &str, bytecode: &[u8]) -> DeployParams {
        DeployParams {
            name: name.to_string(),
            engine: engine.to_string(),
            bytecode: bytecode.to_vec(),
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn token_id_is_deterministic_for_identical_inputs() {
        let h = hash();
        let a = derive_token_id(b"deployer", 7, b"bytecode", "my-contract", &h);
        let b = derive_token_id(b"deployer", 7, b"bytecode", "my-contract", &h);
        assert_eq!(a, b);
        // 32-byte SHA-256 id.
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn token_id_changes_with_nonce() {
        let h = hash();
        let a = derive_token_id(b"deployer", 7, b"bytecode", "my-contract", &h);
        let b = derive_token_id(b"deployer", 8, b"bytecode", "my-contract", &h);
        assert_ne!(a, b);
    }

    #[test]
    fn token_id_changes_with_deployer_bytecode_name() {
        let h = hash();
        let base = derive_token_id(b"deployer", 7, b"bytecode", "my-contract", &h);
        assert_ne!(derive_token_id(b"other", 7, b"bytecode", "my-contract", &h), base);
        assert_ne!(derive_token_id(b"deployer", 7, b"other-bytes", "my-contract", &h), base);
        assert_ne!(derive_token_id(b"deployer", 7, b"bytecode", "other-name", &h), base);
    }

    #[test]
    fn deploy_gas_is_base_plus_per_byte() {
        assert_eq!(deploy_gas(0), DEPLOY_GAS_BASE);
        assert_eq!(deploy_gas(10), DEPLOY_GAS_BASE + 100);
        assert_eq!(deploy_gas(1_000_000), DEPLOY_GAS_BASE + 10_000_000);
    }

    #[test]
    fn deploy_contract_builds_delta_with_derived_id() {
        let h = hash();
        let p = params("my-contract", "Spec", b"ast-bytes");
        let delta = deploy_contract(b"deployer", 7, &p, &h).unwrap();
        assert_eq!(delta.token_id, derive_token_id(b"deployer", 7, b"ast-bytes", "my-contract", &h));
        assert_eq!(delta.name, "my-contract");
        assert_eq!(delta.engine, "Spec");
        assert_eq!(delta.bytecode, b"ast-bytes".to_vec());
    }

    #[test]
    fn deploy_contract_rejects_empty_name_and_engine() {
        let h = hash();
        assert!(deploy_contract(b"d", 0, &params("", "Spec", b"x"), &h).is_err());
        assert!(deploy_contract(b"d", 0, &params("n", "", b"x"), &h).is_err());
    }

    #[test]
    fn delta_reconstructs_a_contract_token() {
        let h = hash();
        let p = params("my-contract", "Wasm", b"\x00asm");
        let delta = deploy_contract(b"deployer", 7, &p, &h).unwrap();
        let token = delta.to_token("env-1").unwrap();
        assert_eq!(token.id, delta.token_id);
        assert_eq!(token.metadata["token_type"], "contract");
        assert_eq!(token.metadata["contract_engine"], "Wasm");
        assert_eq!(token.metadata["name"], "my-contract");
        assert_eq!(token.environment_id, "env-1");
        // The bytecode round-trips through the token's asset.
        let contract: SmartContract = token.get_asset().expect("asset present");
        assert_eq!(contract.bytecode, b"\x00asm".to_vec());
        assert_eq!(contract.name, "my-contract");
    }

    #[test]
    fn committer_overrides_sender_forged_metadata() {
        let h = hash();
        let mut p = params("my-contract", "Spec", b"x");
        // A malicious sender tries to set the required keys themselves.
        p.metadata.insert("token_type".to_string(), "not-a-contract".to_string());
        p.metadata.insert("contract_engine".to_string(), "Evil".to_string());
        let delta = deploy_contract(b"deployer", 7, &p, &h).unwrap();
        let token = delta.to_token("env-1").unwrap();
        // The committer-set values win.
        assert_eq!(token.metadata["token_type"], "contract");
        assert_eq!(token.metadata["contract_engine"], "Spec");
    }
}
