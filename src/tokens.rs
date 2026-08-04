use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::ops::Deref;
use serde::{Deserialize, Serialize};
use crate::blocks::{Block, Blockchain};
use crate::data::DataError;
use crate::encoding;
use crate::encoding::serialize_to_bytes_rmp;
use crate::environment::EnvironmentMetadata;
use crate::transactions::{SignedTransaction, TransactionCommit};

/// A token IS its own blockchain — independent parallel ledgers.
#[derive(Serialize, Deserialize, Debug)]
#[derive(Clone)]
pub struct Token {
    /// Unique token identifier
    pub id: Vec<u8>,
    /// Token metadata (name, decimals, etc.)
    pub metadata: HashMap<String, String>,
    /// This token's independent blockchain
    pub blockchain: Blockchain,
    /// Serialized asset data (contract bytecode, user profile, etc.)
    asset_data: Option<Vec<u8>>,
    /// SHA-256 hash of the asset data for integrity verification
    pub asset_hash: Vec<u8>,
    /// Number of confirmations needed before trimming old blocks.
    /// When security_level == chain_count, the chain is never trimmed.
    pub security_level: usize,
    /// Whether this token is self-validated (owner IS the authority).
    /// Self-validated tokens skip Executor and Finalizer entirely.
    pub is_self_verified: bool,
    /// Whether this token can be transferred between addresses.
    pub is_non_transferable: bool,
    /// Name of the validation spec to use for this token's blocks.
    pub block_validation_spec_name: String,
    /// Environment ID this token belongs to.
    pub environment_id: String,
    /// Sequence number for this token (incremented on each commit).
    pub sequence_number: usize,
}

impl Token {
    pub const DEFAULT_SECURITY_LEVEL: usize = 5;

    pub fn new() -> Token {
        Token {
            id: vec![],
            metadata: HashMap::new(),
            blockchain: Blockchain::new(),
            asset_data: None,
            asset_hash: vec![],
            security_level: Self::DEFAULT_SECURITY_LEVEL,
            is_self_verified: false,
            is_non_transferable: false,
            block_validation_spec_name: String::from("SelfSigned"),
            environment_id: String::from("default"),
            sequence_number: 0,
        }
    }

    pub fn from_asset<T>(asset: &T) -> Result<Token, std::io::Error>
    where
        T: Serialize,
    {
        match encoding::serialize_to_bytes_rmp(asset) {
            Ok(data) => {
                Ok(Token {
                    id: vec![],
                    metadata: HashMap::new(),
                    blockchain: Blockchain::new(),
                    asset_data: Some(data),
                    asset_hash: vec![],
                    security_level: Self::DEFAULT_SECURITY_LEVEL,
                    is_self_verified: false,
                    is_non_transferable: false,
                    block_validation_spec_name: String::from("SelfSigned"),
                    environment_id: String::from("default"),
                    sequence_number: 0,
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn with_id(mut self, id: Vec<u8>) -> Self {
        self.id = id;
        self
    }

    pub fn with_is_self_verified(mut self, value: bool) -> Self {
        self.is_self_verified = value;
        self
    }

    pub fn set_metadata(&mut self, key: String, value: String) {
        self.metadata.insert(key, value);
    }

    pub fn get_asset<T>(&self) -> Option<T>
    where
        T: for<'a> Deserialize<'a>,
    {
        let Some(asset) = &self.asset_data else {
            return None;
        };
        match encoding::deserialize_rmp_to::<T>(asset) {
            Ok(a) => Some(a),
            Err(_) => None,
        }
    }

    pub fn get_asset_mut<T>(&self) -> Option<T>
    where
        T: for<'a> Deserialize<'a>,
    {
        let Some(asset) = &self.asset_data else {
            return None;
        };
        match encoding::deserialize_rmp_to::<T>(asset) {
            Ok(a) => Some(a),
            Err(_) => None,
        }
    }

    /// Validate a block against this token using the appropriate
    /// validation spec from the environment metadata.
    pub fn validate_block(
        &self,
        block: &Block,
        env_data: &EnvironmentMetadata,
    ) -> BlockValidationResult {
        if !self.blockchain.validate_next_block(block) {
            return BlockValidationResult::Err(BlockValidationError::InvalidFinalizerSignature);
        }

        // Look up the validation spec by name
        let validator_name = if self.block_validation_spec_name.is_empty() {
            self.metadata
                .get("validator_name")
                .cloned()
                .unwrap_or_default()
        } else {
            self.block_validation_spec_name.clone()
        };

        match env_data.block_validators.get(&validator_name) {
            None => DefaultBlockValidator {}.validate(block, &self),
            Some(v) => v.validate(block, &self),
        }
    }

    /// Create a block from a fully signed transaction.
    pub fn create_block(&self, signed_tx: SignedTransaction) -> Block {
        let prev_hash = match self.blockchain.get_count() {
            0 => signed_tx.leader_hash.clone(),
            _ => self.blockchain.get_current_chain_state().last_hash_in,
        };

        Block {
            signed_trans: signed_tx,
            token_metadata: self.metadata.clone(),
            previous_hash: prev_hash,
            timestamp: chrono::Utc::now().timestamp(),
            current_hash: vec![],
        }
    }

    /// Commit a validated block to the token's blockchain.
    /// If the chain has reached max length and this is not an archiver,
    /// trim the oldest block first.
    pub fn commit_block(
        &mut self,
        mut block: Block,
        is_archiver: bool,
        env_data: &EnvironmentMetadata,
    ) -> Result<TokenCommitResult, BlockCommitError> {
        // Validate the block before committing
        let validation_result = self.validate_block(&block, env_data);
        match validation_result {
            BlockValidationResult::Ok => {}
            BlockValidationResult::Err(e) => {
                return Err(BlockCommitError::BlockValidationError(e));
            }
        }

        // If chain has reached max length, trim oldest block
        if !is_archiver && self.has_reached_max_chain_length() {
            self.blockchain.remove_block();
        }

        // Compute the block's current hash
        block.current_hash = crate::blocks::BlockFactory::create_hash(&block);

        // Add block to the chain
        self.blockchain.add_block(block);

        // Increment sequence number
        self.sequence_number += 1;

        Ok(TokenCommitResult {
            token_id: self.id.clone(),
            new_chain_length: self.blockchain.get_count(),
            sequence_number: self.sequence_number,
        })
    }

    pub fn has_reached_max_chain_length(&self) -> bool {
        self.security_level == self.blockchain.get_count()
    }
}

impl Default for Token {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// TokenFactory — dispatches on asset type
// ---------------------------------------------------------------------------

/// Factory for creating tokens from different asset types.
pub struct TokenFactory;

impl TokenFactory {
    /// Create a token by dispatching on the asset type.
    pub fn mint_token<T>(
        asset: &T,
        id: Vec<u8>,
        metadata: &HashMap<String, String>,
        environment_id: String,
    ) -> Result<Token, Error>
    where
        T: Serialize,
    {
        let mut token = Token::from_asset(asset)?;
        token.id = id;
        token.metadata = metadata.clone();
        token.environment_id = environment_id;

        // Determine if this is a self-verified token
        if let Some(is_self) = metadata.get("is_self_verified") {
            token.is_self_verified = is_self.parse().unwrap_or(false);
        }

        // Determine validation spec name
        if let Some(spec_name) = metadata.get("validator_name") {
            token.block_validation_spec_name = spec_name.clone();
        }

        Ok(token)
    }

    /// Create a user token (simple value transfer).
    pub fn mint_user_token(
        owner: Vec<u8>,
        id: Vec<u8>,
        environment_id: String,
    ) -> Result<Token, Error> {
        let user = User::new(owner);
        let metadata = HashMap::from([
            ("is_self_verified".to_string(), "true".to_string()),
            ("token_type".to_string(), "user".to_string()),
        ]);
        TokenFactory::mint_token(&user, id, &metadata, environment_id)
    }

    /// Create a contract token (smart contract execution).
    pub fn mint_contract_token(
        contract: &SmartContract,
        id: Vec<u8>,
        environment_id: String,
    ) -> Result<Token, Error> {
        let mut metadata = HashMap::new();
        metadata.insert("token_type".to_string(), "contract".to_string());
        metadata.insert("contract_name".to_string(), contract.name.clone());
        TokenFactory::mint_token(contract, id, &metadata, environment_id)
    }

    /// Create a proxy auth token (authorization gateway).
    pub fn mint_proxy_auth_token(
        proxy_auth: &ContractProxyAuthorization,
        id: Vec<u8>,
        environment_id: String,
    ) -> Result<Token, Error> {
        let mut metadata = HashMap::new();
        metadata.insert("token_type".to_string(), "proxy_auth".to_string());
        TokenFactory::mint_token(proxy_auth, id, &metadata, environment_id)
    }
}

// ---------------------------------------------------------------------------
// Token types — concrete types for different asset categories
// ---------------------------------------------------------------------------

/// A smart contract with bytecode and metadata.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SmartContract {
    /// Contract name for identification
    pub name: String,
    /// Serialized contract bytecode
    pub bytecode: Vec<u8>,
    /// Contract version
    pub version: String,
}

/// Authorization for a contract proxy to access resources.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ContractProxyAuthorization {
    /// Proxy contract address
    pub proxy_address: Vec<u8>,
    /// Authorized contract address
    pub contract_address: Vec<u8>,
    /// Authorization scope
    pub scope: Vec<String>,
}

/// A user account with fuel balance and identity.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct User {
    /// User's public key
    pub public_key: Vec<u8>,
    /// Balance of fuel/gas for transaction execution
    pub fuel_balance: u64,
    /// Transaction nonce
    pub nonce: usize,
}

impl User {
    pub fn new(public_key: Vec<u8>) -> Self {
        User {
            public_key,
            fuel_balance: 0,
            nonce: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// BlockValidator — trait for validating blocks
// ---------------------------------------------------------------------------

pub trait BlockValidator: Send + Sync {
    fn validate(&self, block: &Block, token: &Token) -> BlockValidationResult;
}

pub struct DefaultBlockValidator {}

impl BlockValidator for DefaultBlockValidator {
    fn validate(&self, block: &Block, token: &Token) -> BlockValidationResult {
        let _ = (block, token);
        BlockValidationResult::Ok
    }
}

/// Result of committing a block to a token's blockchain.
#[derive(Debug)]
pub struct TokenCommitResult {
    pub token_id: Vec<u8>,
    pub new_chain_length: usize,
    pub sequence_number: usize,
}

// ---------------------------------------------------------------------------
// Block validation error — existing enum, re-exported
// ---------------------------------------------------------------------------

pub struct BlockCommitInfo {
    pub is_archiver: bool,
    pub token_id: Vec<u8>,
    pub env_id: String,
    pub env_slush_partition: String,
    pub trans_data: TransactionCommit,
}

pub enum BlockValidationResult {
    Ok,
    Err(BlockValidationError),
}

#[derive(Debug)]
pub enum BlockValidationError {
    TokenNotFound,
    ImproperBlockFormatting,
    IncorrectExecutorTransactionHash,
    IncorrectExecutorTransactionSignature,
    FinalizedTransactionDataWasModified,
    InvalidFinalizerSignature,
}

#[derive(Debug)]
pub enum BlockCommitError {
    TokenWriteLockPoisoned,
    FromDataError(DataError),
    BlockValidationError(BlockValidationError),
}
