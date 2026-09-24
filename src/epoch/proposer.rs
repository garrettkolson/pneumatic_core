//! `BlockProposer` behind `IBlockProposer`: dequeue a proposer-labelled
//! batch from the pending pool for the epoch leader.

use super::*;

// ---------------------------------------------------------------------------
// BlockProposer — leader constructs blocks from the transaction pool
// ---------------------------------------------------------------------------

/// The leader node that proposes blocks. Holds the leader's identity and
/// stake for inclusion in `SignedTransaction` wrappers.
#[derive(Debug, Clone)]
pub struct BlockProposer {
    /// Leader's public key / address
    pub leader_address: Vec<u8>,
    /// Leader's stake amount
    pub leader_stake: u64,
    /// Leader's genesis block hash
    pub leader_hash: Vec<u8>,
}

impl BlockProposer {
    pub fn new(leader_address: Vec<u8>, leader_stake: u64, leader_hash: Vec<u8>) -> Self {
        BlockProposer {
            leader_address,
            leader_stake,
            leader_hash,
        }
    }
}

/// Trait for proposing batches of transactions. Allows mocking in tests.
pub trait IBlockProposer: Send + Sync {
    /// Propose a batch of up to `limit` validated transactions for a token.
    /// Returns tuples of (original transaction, wrapped SignedTransaction).
    fn propose_batch(
        &self,
        registry: &crate::registry::PendingTransactionRegistry,
        token_id: &[u8],
        limit: usize,
    ) -> Result<Vec<(crate::transactions::Transaction, crate::transactions::SignedTransaction)>, PneumaticError>;
}

impl IBlockProposer for BlockProposer {
    fn propose_batch(
        &self,
        registry: &crate::registry::PendingTransactionRegistry,
        token_id: &[u8],
        limit: usize,
    ) -> Result<Vec<(crate::transactions::Transaction, crate::transactions::SignedTransaction)>, PneumaticError> {
        let tx_ids = registry.dequeue_for_leader(token_id, limit);
        let mut result = Vec::with_capacity(tx_ids.len());
        for tx_id in tx_ids {
            let tx = registry.get_transaction(&tx_id)?;
            let signed = crate::transactions::SignedTransaction {
                shielded: None,
                transaction_id: tx.id.clone(),
                transaction: tx.clone(),
                total_stake: 0, // caller fills in after stake resolution
                total_voters: 0, // caller fills in after voter count
                leader_address: self.leader_address.clone(),
                leader_stake: self.leader_stake,
                leader_hash: self.leader_hash.clone(),
                finalizer_addr: vec![],
                finalizer_sig: crate::transactions::TransactionSignature {
                    transaction_id: vec![],
                    env_id: vec![],
                    transaction_hash: vec![],
                    signature: vec![],
                    current_stake: 0,
                },
                executor_sigs: std::collections::HashMap::new(),
                proposer_key: self.leader_address.clone(),
            };
            result.push((tx, signed));
        }
        Ok(result)
    }
}
