use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::blocks::Block;
use crate::encoding;
use crate::environment::EnvironmentMetadata;

#[derive(Serialize, Deserialize)]
pub struct Token {
    pub metadata: HashMap<String, String>,
    asset_data: Option<Vec<u8>>,
    pub asset_hash: Vec<u8>
    // TODO: have asset data as optional in the case of non-executables
    // TODO: and verify with MD5 hash on non-Archiver nodes.
    // TODO: Archivers should store the full assets
}

impl Token {
    pub fn new() -> Token {
        Token {
            metadata: HashMap::new(),
            asset_data: None,
            asset_hash: vec![]
        }
    }

    pub fn from_asset<T>(asset: &T) -> Result<Token, std::io::Error>
        where T : Serialize
    {
        match encoding::serialize_to_bytes_rmp(asset) {
            Ok(data) => {
                Ok(Token {
                    metadata: HashMap::new(),
                    asset_data: Some(data),
                    asset_hash: vec![] // TODO: actually hash the data
                })
            },
            Err(error) => Err(error)
        }
    }

    pub fn set_metadata(&mut self, key: String, value: String) {
        todo!()
        //self.metadata.insert(key, value);
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

    pub fn validate_block(&self, block: Block, env_data: EnvironmentMetadata) -> BlockValidationResult {
        BlockValidationResult::Ok
    }
}

pub enum BlockValidationResult {
    Ok,
    Err(BlockValidationError)
}

pub enum BlockValidationError {
    InvalidFinalizerSignature
}

// private async Task checkAndCommitTransactionResults(object sender, DataEventArgs<Message> args)
// {
    // if (args.DataObject is not { } message) return;
    // if (!_nodeConfig.EnvironmentMetadata.TryGetValue(message.ChainId, out var metadata)) return;
    //
    // if (await validateTransactionMessage(message, metadata) is { } result)
    // {
    // if (await _nodeConfig.DataSerializer.DeserializeFromBytesAsync<Block>(result.Data) is not
    // { } proposedBlock)
    // return;
    //
    // // TODO: figure out how to get the underlying asset for a new token
    // // TODO: (probably have to pass it in with the ADD transaction)
    // // TODO: AND ADD IT TO THE REPOSITORY NODES (PER PARTITION)
    // var token = await metadata.DataProvider.GetTokenAsync(result.TokenId) ??
    // await TokenFactory.MintToken(metadata, new object());
    // var validationResult = await token.ValidateBlock(proposedBlock, metadata);
    //
    // if (validationResult.BlockIsValid)
    // {
    // var isArchiver = _nodeConfig.NodeFunctionTypes.Any(type => type == NodeRegistryType.Archiver);
    // BlockCommitInfo commitInfo = new(metadata, proposedBlock, result, isArchiver);
    // await token.CommitBlock(commitInfo);
    // await _tokenDistributor.Distribute(token);
    // }
    // else
    // // save validation result for reconciliation at epoch end
    // await _blockServices.QueueBlockForReconciliation(validationResult, metadata);
    // }
    // }
    //
    // private async Task<TransactionCommit?> validateTransactionMessage(Message message, EnvironmentMetadata metadata)
    // {
    // // leaving this here for now, since the gossiper check may have to be modified
    // if (!metadata.AsymmetricalCryptoProvider.CheckMessageSignature(message)) return null;
    //
    // return await _nodeConfig.DataSerializer.DeserializeFromBytesAsync<TransactionCommit>(message.Body);
// }