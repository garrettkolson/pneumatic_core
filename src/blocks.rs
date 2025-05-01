use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::vec;
use chrono::{Utc, prelude::*};
use serde::{Deserialize, Serialize};
use crate::tokens::Token;
use crate::transactions::{SignedTransaction};

#[derive(Serialize, Deserialize, Debug)]
pub struct Block {
    pub signed_trans : SignedTransaction,
    pub token_metadata: HashMap<String, String>,
    pub previous_hash : Vec<u8>,
    pub current_hash : Vec<u8>,
    pub timestamp : i64
}

impl Block {
    pub fn from_transaction(signed: SignedTransaction,
                            blockchain: Blockchain,
                            token: &Token) -> Self {
        let prev_hash = match blockchain.get_count() {
            0 => signed.leader_hash.clone(),
            _ => blockchain.get_current_chain_state().last_hash_in
        };

        Block {
            signed_trans: signed,
            token_metadata: token.metadata.clone(),
            previous_hash: prev_hash,
            timestamp: Utc::now().timestamp(),
            current_hash: vec![]
        }
    }

    fn test_block(prev_hash: Vec<u8>) -> Self {
        let test_transaction = SignedTransaction::test_transaction();
        let mut block = Block {
            signed_trans: test_transaction,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            current_hash: vec![],
            timestamp: Utc::now().timestamp()
        };

        block.current_hash = BlockFactory::create_hash(&block);
        block
    }
}

pub struct BlockFactory {}

impl BlockFactory {
    pub fn create_hash(block: &Block) -> Vec<u8> {
        let mut input = block.previous_hash.clone();

        let mut time_bytes = crate::encoding::serialize_to_bytes_rmp(&block.timestamp)
            .expect("Block timestamp couldn't be serialized.");
        input.append(&mut time_bytes);

        let mut trans_bytes = crate::encoding::serialize_to_bytes_rmp(&block.signed_trans)
            .expect("Block signed transaction couldn't be serialized.");
        input.append(&mut trans_bytes);

        // TODO: need to find a way to include token metadata here, so that can't be modified maliciously
        let mut metadata_bytes = crate::encoding::serialize_to_bytes_rmp(&block.token_metadata)
            .expect("Block token metadata couldn't be serialized.");
        input.append(&mut metadata_bytes);

        // TODO: actually hash these bytes via the crypto module
        input
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Blockchain {
    pub chain: VecDeque<Block>,
}

impl Blockchain {
    pub fn new() -> Self {
        Blockchain {
            chain: VecDeque::new(),
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
        if self.chain.len() == 0 {
            return ChainState::empty();
        }

        let mut prev_block = &self.chain[0];
        let prev_hash = BlockFactory::create_hash(&prev_block);
        let mut valid = prev_block.current_hash == prev_hash;

        for (i, _) in self.chain.iter().enumerate() {
            let next_index = i + 1;
            if !valid { return ChainState::invalid(); }
            else if self.chain.len() == next_index {
                return ChainState::new(true, prev_block);
            }

            valid = self.chain[next_index].previous_hash == prev_block.current_hash &&
                BlockFactory::create_hash(&self.chain[next_index]) == self.chain[next_index].current_hash;

            prev_block = &self.chain[next_index];
        }

        ChainState::invalid()
    }

    pub fn validate_next_block(&self, next_block: &Block) -> bool {
        let current_state = self.get_current_chain_state();
        if !current_state.is_valid { return false; }

        match current_state.last_hash_in.len() {
            0 => false,
            _ => current_state.last_hash_in == next_block.previous_hash &&
                BlockFactory::create_hash(next_block) == next_block.current_hash
        }
    }
}

pub struct ChainState {
    pub is_valid: bool,
    pub last_hash_in: Vec<u8>
}

impl ChainState {
    pub fn invalid() -> Self {
        ChainState {
            is_valid: false,
            last_hash_in: vec![]
        }
    }

    pub fn new(valid: bool, last_block: &Block) -> Self {
        ChainState {
            is_valid: valid,
            last_hash_in: last_block.current_hash.clone()
        }
    }

    pub fn empty() -> Self {
        ChainState {
            is_valid: true,
            last_hash_in: vec![]
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::blocks::Block;

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
        let valid_next_block = Block::test_block(vec![ 23, 42, 43 ]);
        blockchain.add_block(valid_next_block);

        let state = blockchain.get_current_chain_state();

        assert!(state.is_valid);
        assert!(state.last_hash_in.len() > 0);
    }

    #[test]
    fn get_current_chain_state_with_invalid_chain() {
        let mut blockchain = Blockchain::new();

        // Add some invalid blocks to the chain
        let valid_next_block = Block::test_block(vec![ 23, 42, 43 ]);
        blockchain.add_block(valid_next_block);
        let invalid_next_block = Block::test_block(vec![ 23, 42, 43 ]);
        blockchain.add_block(invalid_next_block);

        let state = blockchain.get_current_chain_state();

        assert!(!state.is_valid);
        assert_eq!(state.last_hash_in.len(), 0);
    }

    #[test]
    fn validate_next_block_with_valid_block() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks
        let valid_next_block = Block::test_block(vec![ 23, 42, 43 ]);
        blockchain.add_block(valid_next_block);

        let last_block_hash = blockchain.get_current_chain_state().last_hash_in;

        let valid_next_block = Block::test_block(last_block_hash);

        assert!(blockchain.validate_next_block(&valid_next_block));
    }

    #[test]
    fn validate_next_block_with_invalid_previous_hash() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks
        let valid_next_block = Block::test_block(vec![ 23, 42, 43 ]);
        blockchain.add_block(valid_next_block);

        let invalid_next_block = Block::test_block(vec![ 1, 2, 3 ]);

        assert!(!blockchain.validate_next_block(&invalid_next_block));
    }

    #[test]
    fn validate_next_block_with_invalid_block_hash() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks
        let valid_next_block = Block::test_block(vec![ 23, 42, 43 ]);
        blockchain.add_block(valid_next_block);

        let mut invalid_next_block = Block::test_block(
            blockchain.get_current_chain_state().last_hash_in);

        invalid_next_block.current_hash = vec![ 1, 2, 3 ];

        assert!(!blockchain.validate_next_block(&invalid_next_block));
    }
}