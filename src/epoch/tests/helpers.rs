//! Shared fixtures for the epoch test suite (committer convention:
//! cross-file fixtures live here, pub-ified, re-exporting the module
//! header's test-only types).

use super::super::*;
pub use std::sync::Arc;
pub use crate::blocks::{Block, FinalityStatus};
pub use crate::data::{DataProvider, StubDataProvider};


// --- LeaderSelector tests ---

pub fn make_stake_set(stakes: Vec<(Vec<u8>, u64)>) -> StakeSet {
    StakeSet {
        stakers: stakes.into_iter().collect(),
    }
}

pub fn make_executor_set(executors: Vec<(Vec<u8>, u64)>) -> ExecutorSet {
    ExecutorSet {
        executors: executors.into_iter().collect(),
    }
}

// --- BlockProposer tests ---

pub use crate::registry::PendingTransactionRegistry;
pub use crate::transactions::{Transaction, TransactionValidationResult};

pub fn make_validated_registry() -> PendingTransactionRegistry {
    let registry = PendingTransactionRegistry::new();
    registry.register_pending("tx1".into()).unwrap();
    registry.acquire_transaction("tx1").unwrap();
    if let Ok(mut entry) = registry.get_transaction_mut("tx1") {
        entry.transition_to_validated(
            Transaction {
                id: "tx1".into(), action: "Transfer".into(),
                token_id: vec![1, 2], bid: None, sequence_number: 1,
                sender: vec![10], receiver: vec![20], amount: Some(100),
                timestamp: 1000, result_hash: vec![],
                sender_signature: vec![],
            },
            TransactionValidationResult::valid(
                vec![99],
                crate::errors::TransactionRiskFactor {
                    affected_parties: 2, amount: 100,
                    is_contract: false, is_multi_party: false,
                },
            ),
        );
    }

    registry.register_pending("tx2".into()).unwrap();
    registry.acquire_transaction("tx2").unwrap();
    if let Ok(mut entry) = registry.get_transaction_mut("tx2") {
        entry.transition_to_validated(
            Transaction {
                id: "tx2".into(), action: "Transfer".into(),
                token_id: vec![1, 2], bid: None, sequence_number: 2,
                sender: vec![10], receiver: vec![20], amount: Some(200),
                timestamp: 2000, result_hash: vec![],
                sender_signature: vec![],
            },
            TransactionValidationResult::valid(
                vec![99],
                crate::errors::TransactionRiskFactor {
                    affected_parties: 2, amount: 200,
                    is_contract: false, is_multi_party: false,
                },
            ),
        );
    }

    // Enqueue both into the pool
    registry.enqueue_to_pool("tx1", vec![1, 2], 1, 1000, vec![10]);
    registry.enqueue_to_pool("tx2", vec![1, 2], 2, 2000, vec![10]);

    registry
}

// --- EpochBoundaryDetector tests ---

pub fn make_epoch(num: u64, start: i64, end: i64, leader: Vec<u8>) -> Epoch {
    Epoch {
        start_timestamp: start,
        end_timestamp: end,
        epoch_number: num,
        leader_public_key: leader,
    }
}
