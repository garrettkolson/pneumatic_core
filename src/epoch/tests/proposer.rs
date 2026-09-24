//! BlockProposer tests: empty pool, batch-limit shapes, no re-dequeue, and
//! leader-field propagation.
use super::helpers::*;
use super::super::*;

#[test]
fn proposer_empty_pool_returns_empty_vec() {
    let registry = PendingTransactionRegistry::new();
    let proposer = BlockProposer::new(vec![1], 100, vec![2]);
    let batch = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
    assert!(batch.is_empty());
}

#[test]
fn proposer_batch_size_matches_limit() {
    let registry = make_validated_registry();
    let proposer = BlockProposer::new(vec![1], 100, vec![2]);
    let batch = proposer.propose_batch(&registry, &[1, 2], 2).unwrap();
    assert_eq!(batch.len(), 2);
}

#[test]
fn proposer_fewer_txs_than_limit_returns_available() {
    let registry = make_validated_registry();
    let proposer = BlockProposer::new(vec![1], 100, vec![2]);
    let batch = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
    assert_eq!(batch.len(), 2);
}

#[test]
fn proposer_dequeued_txs_not_returned_again() {
    let registry = make_validated_registry();
    let proposer = BlockProposer::new(vec![1], 100, vec![2]);
    let _batch1 = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
    // Second proposal should be empty since pool was drained
    let batch2 = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
    assert!(batch2.is_empty());
}

#[test]
fn proposer_leader_fields_propagated() {
    let registry = make_validated_registry();
    let proposer = BlockProposer::new(vec![99], 777, vec![42]);
    let batch = proposer.propose_batch(&registry, &[1, 2], 10).unwrap();
    for (_, signed) in batch {
        assert_eq!(signed.leader_address, vec![99]);
        assert_eq!(signed.leader_stake, 777);
        assert_eq!(signed.leader_hash, vec![42]);
    }
}
