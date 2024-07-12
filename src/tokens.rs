use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::ops::Deref;
use serde::{Deserialize, Serialize};
use crate::blocks::{Block, Blockchain};
use crate::data::{DataError};
use crate::encoding;
use crate::encoding::serialize_to_bytes_rmp;
use crate::environment::EnvironmentMetadata;
use crate::transactions::TransactionCommit;

#[derive(Serialize, Deserialize)]
pub struct Token {
    pub metadata: HashMap<String, String>,
    pub blockchain: Blockchain,
    asset_data: Option<Vec<u8>>,
    pub asset_hash: Vec<u8>,
    security_level: usize,
    // TODO: have asset data as optional in the case of non-executables
    // TODO: and verify with MD5 hash on non-Archiver nodes.
    // TODO: Archivers should store the full assets
}

impl Token {
    const DEFAULT_SECURITY_LEVEL: usize = 5;

    pub fn new() -> Token {
        Token {
            metadata: HashMap::new(),
            blockchain: Blockchain::new(),
            asset_data: None,
            asset_hash: vec![],
            security_level: Self::DEFAULT_SECURITY_LEVEL
        }
    }

    pub fn from_asset<T>(asset: &T) -> Result<Token, std::io::Error>
        where T : Serialize
    {
        match encoding::serialize_to_bytes_rmp(asset) {
            Ok(data) => {
                Ok(Token {
                    metadata: HashMap::new(),
                    blockchain: Blockchain::new(),
                    asset_data: Some(data),
                    asset_hash: vec![], // TODO: actually hash the data,
                    security_level: Self::DEFAULT_SECURITY_LEVEL
                })
            },
            Err(error) => Err(error)
        }
    }

    pub fn set_metadata(&mut self, key: String, value: String) {
        self.metadata.insert(key, value);
    }

    pub fn get_asset<T>(&self) -> Option<T>
        where T : for<'a> Deserialize<'a>
    {
        let Some(asset) = &self.asset_data else { return None };
        match encoding::deserialize_rmp_to::<T>(asset) {
            Ok(a) => Some(a),
            Err(_) => None
        }
    }

    pub fn get_asset_mut<T>(&self) -> Option<T>
        where T : for<'a> Deserialize<'a>
    {
        todo!()
    }

    pub fn validate_block(&self, block: &Block, env_data: &EnvironmentMetadata) -> BlockValidationResult {
        if !self.blockchain.validate_next_block(block) {
            return BlockValidationResult::Err(BlockValidationError::InvalidFinalizerSignature)
        }

        // let transaction = &block.signed_trans.transaction;
        // let Ok(_) = serialize_to_bytes_rmp(transaction)
        //     .and_then(|data| {
        //         env_data.asym_crypto_provider.lock()
        //             .and_then(|provider| {
        //                 Ok(provider.check_signature(&block.signed_trans.finalizer_sig.signature, &data))
        //             })
        //     })
        //     else {
        //         return BlockValidationResult::Err(BlockValidationError::FinalizedTransactionDataWasModified);
        //     };

        let default_name = String::new();
        let validator_name = self.metadata.get("validator_name")
            .unwrap_or_else(|| &default_name);

        match env_data.block_validators.get(validator_name) {
            None => DefaultBlockValidator{}.validate(block, &self),
            Some(v) => v.validate(block, &self)
        }
    }

    pub fn has_reached_max_chain_length(&self) -> bool {
        self.security_level == self.blockchain.get_count()
    }
}

pub trait BlockValidator {
    fn validate(&self, block: &Block, token: &Token) -> BlockValidationResult;
}

pub struct DefaultBlockValidator {}

impl BlockValidator for DefaultBlockValidator {
    fn validate(&self, block: &Block, token: &Token) -> BlockValidationResult {
        BlockValidationResult::Ok
    }
}

// TODO: have validator for executed / computed blocks (any requiring Executor processing)

pub struct BlockCommitInfo {
    pub is_archiver: bool,
    pub token_id: Vec<u8>,
    pub env_id: String,
    pub env_slush_partition: String,
    pub trans_data: TransactionCommit
}

pub enum BlockValidationResult {
    Ok,
    Err(BlockValidationError)
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

pub enum BlockCommitError {
    TokenWriteLockPoisoned,
    FromDataError(DataError)
}