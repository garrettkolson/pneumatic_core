use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::ops::Deref;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use crate::blocks::{Block, Blockchain};
use crate::data::{DataError, DataProvider};
use crate::encoding;
use crate::encoding::serialize_to_bytes_rmp;
use crate::environment::{CostModel, EnvironmentMetadata};
use crate::transactions::{SignedTransaction, TransactionCommit};
use crate::registry::{PendingAdminCredit, PendingTransactionRegistry};

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

    /// Return mutable access to the raw serialized asset bytes.
    pub fn asset_mut(&mut self) -> Option<&mut Vec<u8>> {
        self.asset_data.as_mut()
    }

    /// Serialize an asset and store it, replacing any existing asset data.
    pub fn set_asset(&mut self, asset: &impl Serialize) -> Result<(), std::io::Error> {
        let data = encoding::serialize_to_bytes_rmp(asset)?;
        self.asset_data = Some(data);
        Ok(())
    }

    /// Deserialize the asset, call `f` to mutate it, re-serialize the result.
    /// Returns the mutated value, or None if no asset is stored.
    pub fn update_asset<T, F>(&mut self, f: F) -> Option<T>
    where
        T: Serialize + for<'a> Deserialize<'a>,
        F: FnOnce(&mut T),
    {
        let data = self.asset_data.as_mut()?;
        let mut asset: T = encoding::deserialize_rmp_to(data).ok()?;
        f(&mut asset);
        *data = encoding::serialize_to_bytes_rmp(&asset).ok()?;
        Some(asset)
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

/// Arguments for token minting with fee deduction.
/// When provided, `mint_token_full` deducts the minting fee from the
/// owner's fuel balance and records admin tax as a pending credit.
pub struct MintArgs {
    /// Owner's public key (used to look up and deduct from their user record).
    pub owner_key: Vec<u8>,
    /// Initial fuel deposit given to the owner (fuel_balance set before fee deduction).
    pub initial_fuel_deposit: u64,
    /// Gas cost model for calculating the minting fee.
    pub cost_model: CostModel,
    /// Data provider for reading/writing the owner's user record.
    pub data_provider: Arc<dyn DataProvider>,
    /// Partition ID for data store lookups.
    pub partition_id: String,
    /// Registry for recording admin tax credits.
    pub admin_credit_registry: Arc<PendingTransactionRegistry>,
}

/// Result of a token mint operation.
#[derive(Debug)]
pub struct MintResult {
    /// The created token.
    pub token: Token,
    /// ID of the admin tax credit recorded (if fee deduction was performed).
    pub admin_credit_id: Option<String>,
}

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

    /// Create a token with fee deduction from the owner's fuel balance.
    ///
    /// Calculates the minting fee as `base_cost * 10` and deducts it from
    /// the owner's fuel balance. Records `admin_tax = fee * admin_tax_percentage`
    /// as a pending admin credit.
    ///
    /// If the owner's fuel balance is insufficient, returns an `InsufficientGas` error.
    pub fn mint_token_full<T>(
        asset: &T,
        args: &MintArgs,
        id: Vec<u8>,
        metadata: &HashMap<String, String>,
        environment_id: String,
    ) -> Result<MintResult, Error>
    where
        T: Serialize,
    {
        // 1. Create the token
        let token = TokenFactory::mint_token(asset, id.clone(), metadata, environment_id)?;

        // 2. Calculate minting fee
        let mint_multiplier: u64 = 10;
        let minting_fee = args.cost_model.base_cost * mint_multiplier;

        // 3. Load owner from data provider
        let mut user = args.data_provider.get_user(&args.owner_key, &args.partition_id)
            .map_err(|e| Error::new(ErrorKind::Other, format!("{:?}", e)))?;

        // 4. Set initial fuel deposit if provided
        if args.initial_fuel_deposit > 0 {
            user.fuel_balance = args.initial_fuel_deposit;
        }

        // 5. Check sufficient balance
        if user.fuel_balance < minting_fee {
            return Err(Error::new(ErrorKind::Other, "InsufficientGas"));
        }

        // 6. Deduct fee from fuel balance
        user.fuel_balance -= minting_fee;
        args.data_provider.save_user(&args.owner_key, user.clone(), &args.partition_id)
            .map_err(|e| Error::new(ErrorKind::Other, format!("{:?}", e)))?;

        // 7. Record admin tax credit
        let admin_tax = (minting_fee as f64) * args.cost_model.admin_tax_percentage;
        if admin_tax > 0.0 {
            let credit_id = format!("admin_credit_{}_{}", hex::encode(&id), args.owner_key.first().unwrap_or(&0));
            let credit = PendingAdminCredit {
                id: credit_id.clone(),
                admin_public_key: args.cost_model.admin_public_key.clone(),
                amount: admin_tax as u64,
                token_id: id,
            };
            args.admin_credit_registry.record_admin_credit(credit);
            Ok(MintResult {
                token,
                admin_credit_id: Some(credit_id),
            })
        } else {
            Ok(MintResult {
                token,
                admin_credit_id: None,
            })
        }
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

pub use crate::user::User;
pub use crate::user::Account;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::data::StubDataProvider;
    use crate::registry::PendingTransactionRegistry;
    use crate::environment::CostModel;
    use crate::user::User;
    use super::*;

    // --- Helpers ---

    fn make_cost_model() -> CostModel {
        CostModel {
            base_cost: 10,
            global_min_stake: 100,
            admin_public_key: vec![1, 2, 3],
            admin_tax_percentage: 0.02, // 2%
            amount_multiplier: HashMap::new(),
        }
    }

    fn make_mint_args(owner_key: Vec<u8>, initial_fuel: u64) -> MintArgs {
        let data_provider = Arc::new(StubDataProvider::new());
        let admin_registry = Arc::new(PendingTransactionRegistry::new());
        MintArgs {
            owner_key: owner_key.clone(),
            initial_fuel_deposit: initial_fuel,
            cost_model: make_cost_model(),
            data_provider,
            partition_id: String::from("token"),
            admin_credit_registry: admin_registry,
        }
    }

    fn make_test_user_key() -> Vec<u8> {
        vec![0xCA, 0xFE]
    }

    // --- Basic mint_token (backward compatibility) ---

    #[test]
    fn mint_token_creates_user_token_successfully() {
        let id = vec![1, 2, 3];
        let token = TokenFactory::mint_user_token(
            vec![0xAA],
            id,
            String::from("test-env"),
        ).unwrap();

        assert_eq!(token.id, vec![1, 2, 3]);
        assert_eq!(token.environment_id, "test-env");
        assert!(token.is_self_verified);
        assert_eq!(token.metadata.get("token_type").unwrap(), "user");
        let user: User = token.get_asset().unwrap();
        assert_eq!(user.public_key, vec![0xAA]);
    }

    #[test]
    fn mint_token_creates_contract_token() {
        let contract = SmartContract {
            name: String::from("MyContract"),
            bytecode: vec![0x01, 0x02],
            version: String::from("1.0"),
        };
        let id = vec![4, 5, 6];
        let token = TokenFactory::mint_contract_token(
            &contract,
            id,
            String::from("test-env"),
        ).unwrap();

        assert_eq!(token.id, vec![4, 5, 6]);
        assert_eq!(token.metadata.get("contract_name").unwrap(), "MyContract");
        let sc: SmartContract = token.get_asset().unwrap();
        assert_eq!(sc.name, "MyContract");
    }

    // --- mint_token_full — fee deduction ---

    #[test]
    fn mint_token_full_deducts_fee_from_balance() {
        let owner_key = make_test_user_key();
        let base_cost = 10u64;
        let initial_fuel = 100u64;
        let expected_fee = base_cost * 10; // 100
        let expected_remaining = initial_fuel - expected_fee; // 0

        let mut args = make_mint_args(owner_key.clone(), initial_fuel);
        args.cost_model.base_cost = base_cost;

        let user = User::new(owner_key.clone());
        args.data_provider = Arc::new(
            StubDataProvider::new().with_user(owner_key.clone(), String::from("token"), user),
        );

        let asset = User::new(owner_key.clone());
        let metadata = HashMap::from([
            ("is_self_verified".to_string(), "true".to_string()),
            ("token_type".to_string(), "user".to_string()),
        ]);

        let result = TokenFactory::mint_token_full(
            &asset,
            &args,
            vec![1],
            &metadata,
            String::from("test"),
        ).unwrap();

        // Fee should have been deducted
        let updated_user = args.data_provider.get_user(&owner_key, "token").unwrap();
        assert_eq!(updated_user.fuel_balance, expected_remaining);
        // Token should be created
        assert!(!result.token.id.is_empty());
    }

    #[test]
    fn mint_token_full_records_admin_tax_credit() {
        let owner_key = make_test_user_key();
        let base_cost = 10u64;
        let initial_fuel = 200u64;
        let admin_tax_pct = 0.02; // 2%
        let expected_tax = (base_cost * 10) as f64 * admin_tax_pct; // 2.0 → 2

        let mut args = make_mint_args(owner_key.clone(), initial_fuel);
        args.cost_model.base_cost = base_cost;
        args.cost_model.admin_tax_percentage = admin_tax_pct;

        let user = User::new(owner_key.clone());
        args.data_provider = Arc::new(
            StubDataProvider::new().with_user(owner_key.clone(), String::from("token"), user),
        );

        let asset = User::new(owner_key.clone());
        let metadata = HashMap::from([
            ("is_self_verified".to_string(), "true".to_string()),
            ("token_type".to_string(), "user".to_string()),
        ]);

        let result = TokenFactory::mint_token_full(
            &asset,
            &args,
            vec![1],
            &metadata,
            String::from("test"),
        ).unwrap();

        // Admin tax credit should be recorded
        assert!(result.admin_credit_id.is_some());
        let credit_id = result.admin_credit_id.unwrap();
        let credit = args.admin_credit_registry.get_admin_credit(&credit_id).unwrap();
        assert_eq!(credit.amount, expected_tax as u64);
        assert_eq!(credit.admin_public_key, vec![1, 2, 3]);
    }

    #[test]
    fn mint_token_full_no_admin_tax_when_zero_percentage() {
        let owner_key = make_test_user_key();
        let initial_fuel = 200u64;

        let mut args = make_mint_args(owner_key.clone(), initial_fuel);
        args.cost_model.admin_tax_percentage = 0.0;

        let user = User::new(owner_key.clone());
        args.data_provider = Arc::new(
            StubDataProvider::new().with_user(owner_key.clone(), String::from("token"), user),
        );

        let asset = User::new(owner_key.clone());
        let metadata = HashMap::from([
            ("is_self_verified".to_string(), "true".to_string()),
            ("token_type".to_string(), "user".to_string()),
        ]);

        let result = TokenFactory::mint_token_full(
            &asset,
            &args,
            vec![1],
            &metadata,
            String::from("test"),
        ).unwrap();

        // No admin credit when tax percentage is zero
        assert!(result.admin_credit_id.is_none());
    }

    #[test]
    fn mint_token_full_insufficient_gas_error() {
        let owner_key = make_test_user_key();
        let base_cost = 10u64;
        let initial_fuel = 50u64; // fee = 100, balance = 50 → insufficient

        let mut args = make_mint_args(owner_key.clone(), initial_fuel);
        args.cost_model.base_cost = base_cost;

        let user = User::new(owner_key.clone());
        args.data_provider = Arc::new(
            StubDataProvider::new().with_user(owner_key.clone(), String::from("token"), user),
        );

        let asset = User::new(owner_key.clone());
        let metadata = HashMap::from([
            ("is_self_verified".to_string(), "true".to_string()),
            ("token_type".to_string(), "user".to_string()),
        ]);

        let result = TokenFactory::mint_token_full(
            &asset,
            &args,
            vec![1],
            &metadata,
            String::from("test"),
        );

        assert!(result.is_err());
    }

    #[test]
    fn mint_token_full_zero_base_cost_no_deduction() {
        let owner_key = make_test_user_key();
        let initial_fuel = 100u64;

        let mut args = make_mint_args(owner_key.clone(), initial_fuel);
        args.cost_model.base_cost = 0;

        let user = User::new(owner_key.clone());
        args.data_provider = Arc::new(
            StubDataProvider::new().with_user(owner_key.clone(), String::from("token"), user),
        );

        let asset = User::new(owner_key.clone());
        let metadata = HashMap::from([
            ("is_self_verified".to_string(), "true".to_string()),
            ("token_type".to_string(), "user".to_string()),
        ]);

        let result = TokenFactory::mint_token_full(
            &asset,
            &args,
            vec![1],
            &metadata,
            String::from("test"),
        ).unwrap();

        // Zero fee → balance unchanged
        assert_eq!(result.token.id, vec![1]);
        let updated_user = args.data_provider.get_user(&owner_key, "token").unwrap();
        assert_eq!(updated_user.fuel_balance, initial_fuel);
        // No admin credit (tax of 0)
        assert!(result.admin_credit_id.is_none());
    }

    #[test]
    fn mint_token_full_admin_credit_taken() {
        let owner_key = make_test_user_key();
        let initial_fuel = 200u64;

        let mut args = make_mint_args(owner_key.clone(), initial_fuel);
        args.cost_model.admin_tax_percentage = 0.1; // 10%

        let user = User::new(owner_key.clone());
        args.data_provider = Arc::new(
            StubDataProvider::new().with_user(owner_key.clone(), String::from("token"), user),
        );

        let asset = User::new(owner_key.clone());
        let metadata = HashMap::from([
            ("is_self_verified".to_string(), "true".to_string()),
            ("token_type".to_string(), "user".to_string()),
        ]);

        let result = TokenFactory::mint_token_full(
            &asset,
            &args,
            vec![42],
            &metadata,
            String::from("test"),
        ).unwrap();

        let credit_id = result.admin_credit_id.unwrap();

        // Credit should exist and be retrievable
        assert!(args.admin_credit_registry.get_admin_credit(&credit_id).is_some());

        // Taking the credit should remove it
        let taken = args.admin_credit_registry.take_admin_credit(&credit_id);
        assert!(taken.is_some());
        assert!(args.admin_credit_registry.get_admin_credit(&credit_id).is_none());
    }

    // --- asset_mut / set_asset / update_asset ---

    #[test]
    fn asset_mut_returns_none_when_no_asset() {
        let mut token = Token::new();
        assert!(token.asset_mut().is_none());
    }

    #[test]
    fn asset_mut_returns_mutable_ref_when_asset_exists() {
        let mut token = Token::from_asset(&User::new(vec![0xAA])).unwrap();
        let data = token.asset_mut().unwrap();
        assert!(!data.is_empty());
        let len_before = data.len();
        data.push(0xFF);
        assert_eq!(token.asset_mut().unwrap().len(), len_before + 1);
    }

    #[test]
    fn set_asset_serializes_and_stores() {
        let mut token = Token::new();
        let user = User::new(vec![0xBB]);
        assert!(token.set_asset(&user).is_ok());
        let retrieved: User = token.get_asset().unwrap();
        assert_eq!(retrieved.public_key, vec![0xBB]);
    }

    #[test]
    fn update_asset_mutates_and_reserializes() {
        let mut token = Token::from_asset(&User::new(vec![0xCC])).unwrap();
        let updated = token.update_asset(|user: &mut User| {
            user.fuel_balance = 500;
            user.stake = 1000;
        }).unwrap();
        assert_eq!(updated.fuel_balance, 500);
        assert_eq!(updated.stake, 1000);
        assert_eq!(updated.public_key, vec![0xCC]);
        // Verify stored data reflects the mutation
        let stored: User = token.get_asset().unwrap();
        assert_eq!(stored.fuel_balance, 500);
        assert_eq!(stored.stake, 1000);
    }

    #[test]
    fn update_asset_returns_none_when_no_asset() {
        let mut token = Token::new();
        let result: Option<User> = token.update_asset(|_user: &mut User| {});
        assert!(result.is_none());
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
