use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::vec;
use chrono::{Utc, prelude::*};
use serde::{Deserialize, Serialize};
use crate::crypto::{BasicHashProvider, HashProvider};
use crate::tokens::Token;
use crate::transactions::SignedTransaction;

/// Finality status of a block — tracks whether a block could still be superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinalityStatus {
    /// Optimistic: block was committed without conflict but could theoretically be
    /// overridden if a higher-stake competing proposal is later detected.
    Optimistic,
    /// Confirmed: block has been observed with no conflict for enough time to be
    /// considered irreversible.
    Confirmed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Block {
    pub signed_trans: SignedTransaction,
    pub token_metadata: HashMap<String, String>,
    pub previous_hash: Vec<u8>,
    pub current_hash: Vec<u8>,
    pub timestamp: i64,
    /// Whether this block is optimistically committed or confirmed.
    pub finality_status: FinalityStatus,
    /// Public key of the proposer who created this block.
    pub proposer_key: Vec<u8>,
    /// Epoch number at which this block was proposed.
    /// Used by downstream nodes to verify deterministic routing decisions.
    pub epoch_number: u64,
}

impl Block {
    pub fn from_transaction(
        signed: SignedTransaction,
        blockchain: Blockchain,
        token: &Token,
        epoch_number: u64,
    ) -> Self {
        let prev_hash = match blockchain.get_count() {
            // Genesis convention: block 1 has an empty previous_hash.
            0 => Vec::<u8>::new(),
            _ => blockchain.get_current_chain_state().last_hash_in,
        };

        Block {
            signed_trans: signed.clone(),
            token_metadata: token.metadata.clone(),
            previous_hash: prev_hash,
            timestamp: Utc::now().timestamp(),
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: signed.leader_address.clone(),
            epoch_number,
        }
    }

    pub(crate) fn test_block(prev_hash: Vec<u8>) -> Self {
        let test_transaction = SignedTransaction::test_transaction();
        let mut block = Block {
            signed_trans: test_transaction,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            current_hash: vec![],
            timestamp: Utc::now().timestamp(),
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };

        block.current_hash = BlockFactory::create_hash(&block);
        block
    }
}

pub struct BlockFactory {}

impl BlockFactory {
    /// Create a block hash using SHA-256 from the crypto module.
    /// Hashes: previous_hash || timestamp || signed_transaction || token_metadata
    pub fn create_hash(block: &Block) -> Vec<u8> {
        let mut input = block.previous_hash.clone();

        let mut time_bytes = crate::encoding::serialize_to_bytes_rmp(&block.timestamp)
            .expect("Block timestamp couldn't be serialized.");
        input.append(&mut time_bytes);

        let mut trans_bytes = crate::encoding::serialize_to_bytes_rmp(&block.signed_trans)
            .expect("Block signed transaction couldn't be serialized.");
        input.append(&mut trans_bytes);

        let mut metadata_bytes = crate::encoding::serialize_to_bytes_rmp(&block.token_metadata)
            .expect("Block token metadata couldn't be serialized.");
        input.append(&mut metadata_bytes);

        BasicHashProvider::new().hash(&input)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Blockchain {
    pub chain: VecDeque<Block>,
    /// Metadata about the blockchain (e.g., genesis hash, chain name)
    pub metadata: HashMap<String, String>,
}

impl Blockchain {
    pub fn new() -> Self {
        Blockchain {
            chain: VecDeque::new(),
            metadata: HashMap::new(),
        }
    }

    pub fn get_count(&self) -> usize {
        self.chain.len()
    }

    pub fn add_block(&mut self, block: Block) {
        self.chain.push_back(block);
    }

    pub fn remove_block(&mut self) -> Option<Block> {
        self.chain.pop_front()
    }

    pub fn get_current_chain_state(&self) -> ChainState {
        if self.chain.is_empty() {
            return ChainState::empty();
        }

        let mut prev_block = &self.chain[0];
        let prev_hash = BlockFactory::create_hash(&prev_block);
        let mut valid = prev_block.current_hash == prev_hash;

        for (i, _) in self.chain.iter().enumerate() {
            let next_index = i + 1;
            if !valid {
                return ChainState::invalid();
            } else if self.chain.len() == next_index {
                return ChainState::new(true, prev_block);
            }

            valid = self.chain[next_index].previous_hash == prev_block.current_hash
                && BlockFactory::create_hash(&self.chain[next_index]) == self.chain[next_index].current_hash;

            prev_block = &self.chain[next_index];
        }

        ChainState::invalid()
    }

    pub fn validate_next_block(&self, next_block: &Block) -> bool {
        let current_state = self.get_current_chain_state();
        if !current_state.is_valid {
            return false;
        }

        // Genesis convention: block 1 has an empty previous_hash, matching
        // ChainState::empty().last_hash_in (which is also vec![]).
        let linkage_ok = if current_state.last_hash_in.is_empty() {
            next_block.previous_hash.is_empty()
        } else {
            current_state.last_hash_in == next_block.previous_hash
        };

        linkage_ok && BlockFactory::create_hash(next_block) == next_block.current_hash
    }

    /// Get a block by index. Returns None if out of range.
    pub fn get_block_at(&self, index: usize) -> Option<&Block> {
        self.chain.get(index)
    }

    /// Set the finality status of a block by hash.
    /// Returns Ok(()) if found, Err(()) if block hash not in chain.
    pub fn set_finality_status(
        &mut self,
        block_hash: &[u8],
        status: FinalityStatus,
    ) -> Result<(), ()> {
        for block in self.chain.iter_mut() {
            if block.current_hash == block_hash {
                block.finality_status = status;
                return Ok(());
            }
        }
        Err(())
    }
}

#[derive(Clone)]
pub struct ChainState {
    pub is_valid: bool,
    pub last_hash_in: Vec<u8>,
}

impl ChainState {
    pub fn invalid() -> Self {
        ChainState {
            is_valid: false,
            last_hash_in: vec![],
        }
    }

    pub fn new(valid: bool, last_block: &Block) -> Self {
        ChainState {
            is_valid: valid,
            last_hash_in: last_block.current_hash.clone(),
        }
    }

    pub fn empty() -> Self {
        ChainState {
            is_valid: true,
            last_hash_in: vec![],
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::blocks::Block;

    // --- FinalityStatus tests ---

    #[test]
    fn finality_status_optimistic_deserializes() {
        let status = FinalityStatus::Optimistic;
        let bytes = crate::encoding::serialize_to_bytes_rmp(&status).unwrap();
        let recovered: FinalityStatus = crate::encoding::deserialize_rmp_to(&bytes).unwrap();
        assert_eq!(recovered, FinalityStatus::Optimistic);
    }

    #[test]
    fn finality_status_confirmed_deserializes() {
        let status = FinalityStatus::Confirmed;
        let bytes = crate::encoding::serialize_to_bytes_rmp(&status).unwrap();
        let recovered: FinalityStatus = crate::encoding::deserialize_rmp_to(&bytes).unwrap();
        assert_eq!(recovered, FinalityStatus::Confirmed);
    }

    #[test]
    fn finality_status_block_default_is_optimistic() {
        let tx = SignedTransaction::test_transaction();
        let blockchain = Blockchain::new();
        let token = Token::test_token();
        let block = Block::from_transaction(tx, blockchain, &token, 0);
        assert_eq!(block.finality_status, FinalityStatus::Optimistic);
    }

    #[test]
    fn block_test_block_has_optimistic_finality() {
        let block = Block::test_block(vec![1, 2, 3]);
        assert_eq!(block.finality_status, FinalityStatus::Optimistic);
    }

    #[test]
    fn from_transaction_empty_blockchain_yields_empty_previous_hash() {
        let tx = SignedTransaction::test_transaction();
        let blockchain = Blockchain::new();
        let token = Token::test_token();

        let block = Block::from_transaction(tx, blockchain, &token, 0);

        // Genesis convention: block 1 has an empty previous_hash
        assert!(block.previous_hash.is_empty());
    }

    #[test]
    fn get_current_chain_state_with_empty_chain() {
        let blockchain = Blockchain::new();

        let state = blockchain.get_current_chain_state();

        assert!(state.is_valid);
        assert_eq!(state.last_hash_in.len(), 0);
    }

    #[test]
    fn get_current_chain_state_with_valid_chain() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks to the chain
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);

        let state = blockchain.get_current_chain_state();

        assert!(state.is_valid);
        assert!(state.last_hash_in.len() > 0);
    }

    #[test]
    fn get_current_chain_state_with_invalid_chain() {
        let mut blockchain = Blockchain::new();

        // Add some invalid blocks to the chain
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);
        let invalid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(invalid_next_block);

        let state = blockchain.get_current_chain_state();

        assert!(!state.is_valid);
        assert_eq!(state.last_hash_in.len(), 0);
    }

    #[test]
    fn validate_next_block_with_valid_block() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);

        let last_block_hash = blockchain.get_current_chain_state().last_hash_in;

        let valid_next_block = Block::test_block(last_block_hash);

        assert!(blockchain.validate_next_block(&valid_next_block));
    }

    #[test]
    fn validate_next_block_with_invalid_previous_hash() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);

        let invalid_next_block = Block::test_block(vec![1, 2, 3]);

        assert!(!blockchain.validate_next_block(&invalid_next_block));
    }

    #[test]
    fn validate_next_block_with_invalid_block_hash() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);

        let mut invalid_next_block = Block::test_block(
            blockchain.get_current_chain_state().last_hash_in,
        );

        invalid_next_block.current_hash = vec![1, 2, 3];

        assert!(!blockchain.validate_next_block(&invalid_next_block));
    }

    #[test]
    fn validate_next_block_empty_chain_accepts_genesis_block() {
        let blockchain = Blockchain::new();

        // Genesis convention: block 1 has an empty previous_hash
        let genesis_block = Block::test_block(Vec::<u8>::new());

        assert!(blockchain.validate_next_block(&genesis_block));
    }

    #[test]
    fn validate_next_block_empty_chain_rejects_nonempty_previous_hash() {
        let blockchain = Blockchain::new();

        let block = Block::test_block(vec![1, 2, 3]);

        assert!(!blockchain.validate_next_block(&block));
    }

    #[test]
    fn validate_next_block_empty_chain_rejects_invalid_block_hash() {
        let blockchain = Blockchain::new();

        let mut genesis_block = Block::test_block(Vec::<u8>::new());
        genesis_block.current_hash = vec![1, 2, 3];

        assert!(!blockchain.validate_next_block(&genesis_block));
    }
}
