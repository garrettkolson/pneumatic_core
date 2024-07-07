use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::blocks::Block;

#[derive(Serialize, Deserialize)]
pub struct Transaction {

}

#[derive(Serialize, Deserialize)]
pub struct TransactionSignature {

}

#[derive(Serialize, Deserialize)]
pub struct SignedTransaction {
    pub transaction_id : String,
    pub transaction : Transaction,
    pub total_stake : u64,
    pub total_voters : u32,
    pub leader_address : Vec<u8>,
    pub leader_stake : u64,
    pub leader_hash : Vec<u8>,
    pub finalizer_addr: Vec<u8>,
    pub finalizer_sig: TransactionSignature,
    pub executor_sigs: HashMap<Vec<u8>, TransactionSignature>
}

impl SignedTransaction {
    pub fn test_transaction() -> Self {
        SignedTransaction {
            transaction_id: String::from("test_transaction"),
            transaction: Transaction {},
            total_stake: 42,
            total_voters: 3,
            leader_address: vec![],
            leader_stake: 24,
            leader_hash: vec![],
            finalizer_addr: vec![],
            finalizer_sig: TransactionSignature {},
            executor_sigs: HashMap::new()
        }
    }
}

#[derive (Serialize, Deserialize)]
pub struct TransactionCommit {
    pub proposed_block: Block
}