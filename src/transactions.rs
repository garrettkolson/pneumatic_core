use std::collections::{BTreeMap, HashMap};
use serde::{Deserialize, Serialize};
use crate::blocks::Block;
use crate::errors::{ValidationFailureReason, TransactionRiskFactor};

// ---------------------------------------------------------------------------
// TransactionState — explicit lifecycle through the pipeline
// ---------------------------------------------------------------------------

/// Explicit transaction state machine. A PendingTransaction holds its
/// current state rather than relying on optional fields.
#[derive(Debug, Clone)]
pub enum TransactionState {
    /// Initial state when transaction first enters the system
    Pending,
    /// Preloaded: data (users, token, contract) fetched from DataProvider
    Preloaded {
        /// The deserialized transaction payload
        transaction: Transaction,
    },
    /// Validated: passed Sentinel checks (nonce, gas, signature, risk)
    Validated {
        transaction: Transaction,
        /// Validation result from the appropriate TransactionValidationSpec
        validation: TransactionValidationResult,
    },
    /// Being executed by Executor (in-flight)
    Executing {
        transaction: Transaction,
    },
    /// Finalizer is collecting signatures toward quorum
    Finalizing {
        transaction: Transaction,
        /// Public key of the assigned finalizer node
        finalizer_key: Vec<u8>,
    },
    /// Transaction committed to a block
    Committed {
        transaction: Transaction,
        /// Hash of the block containing this transaction
        block_hash: Vec<u8>,
    },
    /// Transaction failed validation or execution
    Failed {
        transaction: Transaction,
        /// Reasons for the failure
        reasons: Vec<ValidationFailureReason>,
    },
}

impl TransactionState {
    pub fn is_pending(&self) -> bool {
        matches!(self, TransactionState::Pending)
    }

    pub fn is_finalizing(&self) -> bool {
        matches!(self, TransactionState::Finalizing { .. })
    }

    pub fn transaction(&self) -> Option<&Transaction> {
        match self {
            TransactionState::Preloaded { transaction }
            | TransactionState::Validated { transaction, .. }
            | TransactionState::Executing { transaction }
            | TransactionState::Finalizing { transaction, .. }
            | TransactionState::Committed { transaction, .. }
            | TransactionState::Failed { transaction, .. } => Some(transaction),
            TransactionState::Pending => None,
        }
    }
}

// ---------------------------------------------------------------------------
// PendingTransaction — wraps state with concurrency controls
// ---------------------------------------------------------------------------

/// Transaction in-flight through the pipeline. Holds an explicit state and
/// a lock count to prevent premature collection during multi-stage transit.
#[derive(Debug)]
pub struct PendingTransaction {
    pub id: String,
    pub state: TransactionState,
    /// Lock count incremented when the transaction is handed to a new stage
    /// and decremented when the stage releases it. When 0, the transaction
    /// can be safely removed from the registry.
    lock_count: usize,
}

impl PendingTransaction {
    pub fn new(id: String, state: TransactionState) -> Self {
        PendingTransaction { id, state, lock_count: 0 }
    }

    /// Acquire a lock on this transaction for a new pipeline stage.
    /// Returns `Ok(())` if the transaction is still active, `Err` if failed/committed.
    pub fn acquire(&mut self) -> Result<(), ()> {
        match &self.state {
            TransactionState::Failed { .. } | TransactionState::Committed { .. } => Err(()),
            _ => {
                self.lock_count += 1;
                Ok(())
            }
        }
    }

    /// Release a lock and drop the transaction if lock_count reaches 0.
    /// Returns `true` if the transaction should be removed from the registry.
    pub fn release(&mut self) -> bool {
        if self.lock_count > 0 {
            self.lock_count -= 1;
        }
        self.lock_count == 0
            && matches!(
                &self.state,
                TransactionState::Failed { .. } | TransactionState::Committed { .. }
            )
    }

    /// Transition to Preloaded state.
    pub fn transition_to_preloaded(&mut self, tx: Transaction) {
        self.state = TransactionState::Preloaded { transaction: tx };
    }

    /// Transition to Validated state.
    pub fn transition_to_validated(&mut self, tx: Transaction, validation: TransactionValidationResult) {
        self.state = TransactionState::Validated { transaction: tx, validation };
    }

    /// Transition to Executing state.
    pub fn transition_to_executing(&mut self, tx: Transaction) {
        self.state = TransactionState::Executing { transaction: tx };
    }

    /// Transition to Finalizing state with assigned finalizer.
    pub fn transition_to_finalizing(&mut self, tx: Transaction, finalizer_key: Vec<u8>) {
        self.state = TransactionState::Finalizing { transaction: tx, finalizer_key };
    }

    /// Transition to Committed state.
    pub fn transition_to_committed(&mut self, tx: Transaction, block_hash: Vec<u8>) {
        self.state = TransactionState::Committed { transaction: tx, block_hash };
    }

    /// Transition to Failed state with reasons.
    pub fn transition_to_failed(&mut self, tx: Transaction, reasons: Vec<ValidationFailureReason>) {
        self.state = TransactionState::Failed { transaction: tx, reasons };
    }

    /// Check if transaction is awaiting finalizer assignment.
    pub fn is_awaiting_finalizer(&self) -> bool {
        matches!(&self.state, TransactionState::Validated { .. })
    }
}

// ---------------------------------------------------------------------------
// Transaction — full model with all protocol fields
// ---------------------------------------------------------------------------

/// Represents a pending transaction in the pipeline.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Transaction {
    /// Unique transaction identifier
    pub id: String,
    /// Action to perform (e.g., "Transfer", "ContractCall")
    pub action: String,
    /// Token/blockchain this transaction targets
    pub token_id: Vec<u8>,
    /// Optional bid with expiry and percentage
    pub bid: Option<Bid>,
    /// Sender's sequence number (nonce) for this token
    pub sequence_number: usize,
    /// Sender's public key bytes
    pub sender: Vec<u8>,
    /// Receiver's public key bytes (empty for contract calls)
    pub receiver: Vec<u8>,
    /// Transaction amount
    pub amount: Option<u64>,
    /// Timestamp of creation
    pub timestamp: i64,
    /// Hash of execution result (set by Executor)
    pub result_hash: Vec<u8>,
}

/// Optional bid attached to a transaction.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Bid {
    /// Expiry timestamp after which the bid is invalid
    pub bid_expiry: i64,
    /// Percentage of the transaction amount offered as a bid/rate
    pub bid_percentage: f32,
}

// ---------------------------------------------------------------------------
// TransactionValidationResult — output from TransactionValidationSpec
// ---------------------------------------------------------------------------

/// Result returned by validating a transaction against its spec.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TransactionValidationResult {
    /// Whether the transaction passed all validation checks
    pub is_valid: bool,
    /// Computed risk metrics for the transaction
    pub risk: TransactionRiskFactor,
    /// Reasons for failure (empty if valid)
    pub failure_reasons: Vec<ValidationFailureReason>,
    /// Public key of the finalizer to handle this transaction
    pub finalizer_public_key: Vec<u8>,
}

impl TransactionValidationResult {
    pub fn valid(finalizer_key: Vec<u8>, risk: TransactionRiskFactor) -> Self {
        TransactionValidationResult {
            is_valid: true,
            risk,
            failure_reasons: vec![],
            finalizer_public_key: finalizer_key,
        }
    }

    pub fn invalid(reasons: Vec<ValidationFailureReason>) -> Self {
        TransactionValidationResult {
            is_valid: false,
            risk: TransactionRiskFactor {
                affected_parties: 0,
                amount: 0,
                is_contract: false,
                is_multi_party: false,
            },
            failure_reasons: reasons.clone(),
            finalizer_public_key: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// TransactionSignature — signature from executor or finalizer
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TransactionSignature {
    pub transaction_id: Vec<u8>,
    pub env_id: Vec<u8>,
    pub transaction_hash: Vec<u8>,
    pub signature: Vec<u8>,
    pub current_stake: u64,
}

// ---------------------------------------------------------------------------
// SignedTransaction — fully signed, ready for commitment
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SignedTransaction {
    /// Unique transaction ID
    pub transaction_id: String,
    /// The transaction payload
    pub transaction: Transaction,
    /// Total stake of all voting nodes
    pub total_stake: u64,
    /// Total number of voters in the environment
    pub total_voters: u32,
    /// Leader's address/public key
    pub leader_address: Vec<u8>,
    /// Leader's stake amount
    pub leader_stake: u64,
    /// Leader's genesis block hash
    pub leader_hash: Vec<u8>,
    /// Finalizer address
    pub finalizer_addr: Vec<u8>,
    /// Finalizer's signature over the transaction
    pub finalizer_sig: TransactionSignature,
    /// Executor signatures keyed by executor public key
    pub executor_sigs: HashMap<Vec<u8>, TransactionSignature>,
    /// Public key of the proposer who created this transaction/block.
    /// Used for conflict-resolution stake lookup.
    pub proposer_key: Vec<u8>,
}

impl SignedTransaction {
    pub fn test_transaction() -> Self {
        SignedTransaction {
            transaction_id: String::from("test_transaction"),
            transaction: Transaction {
                id: String::from("test_transaction"),
                action: String::from("Transfer"),
                token_id: vec![],
                bid: None,
                sequence_number: 0,
                sender: vec![],
                receiver: vec![],
                amount: None,
                timestamp: 0,
                result_hash: vec![],
            },
            total_stake: 42,
            total_voters: 3,
            leader_address: vec![],
            leader_stake: 24,
            leader_hash: vec![],
            finalizer_addr: vec![],
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: vec![],
                signature: vec![],
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical serialization for the block hash
// ---------------------------------------------------------------------------

/// Deterministic, order-independent serialization of a `SignedTransaction`, used by
/// `BlockFactory::create_hash`.
///
/// `SignedTransaction`'s only non-deterministic field is `executor_sigs`, a `std` `HashMap` whose
/// iteration order is random-seeded. This serializes a view whose `executor_sigs` is a `BTreeMap`
/// (sorted by executor public key), so two equal `SignedTransaction`s always produce identical
/// bytes regardless of insertion order or a serde round-trip. The real `Serialize` impl of
/// `SignedTransaction` is left untouched — this is hashing-only, it does not change the wire form.
#[derive(Serialize)]
struct CanonicalSignedTransaction<'a> {
    transaction_id: &'a str,
    transaction: &'a Transaction,
    total_stake: u64,
    total_voters: u32,
    leader_address: &'a [u8],
    leader_stake: u64,
    leader_hash: &'a [u8],
    finalizer_addr: &'a [u8],
    finalizer_sig: &'a TransactionSignature,
    executor_sigs: BTreeMap<&'a Vec<u8>, &'a TransactionSignature>,
    proposer_key: &'a [u8],
}

/// Serialize a `SignedTransaction` in canonical (sorted-key) form for block hashing.
pub(crate) fn canonical_signed_trans_bytes(
    tx: &SignedTransaction,
) -> Result<Vec<u8>, std::io::Error> {
    let cst = CanonicalSignedTransaction {
        transaction_id: &tx.transaction_id,
        transaction: &tx.transaction,
        total_stake: tx.total_stake,
        total_voters: tx.total_voters,
        leader_address: &tx.leader_address,
        leader_stake: tx.leader_stake,
        leader_hash: &tx.leader_hash,
        finalizer_addr: &tx.finalizer_addr,
        finalizer_sig: &tx.finalizer_sig,
        // BTreeMap collects the HashMap entries sorted by public key — mirroring the finalizer's
        // own canonical signature ordering (finalizer/src/block_builder.rs:141-142).
        executor_sigs: tx.executor_sigs.iter().collect(),
        proposer_key: &tx.proposer_key,
    };
    crate::encoding::serialize_to_bytes_rmp(&cst)
}

// ---------------------------------------------------------------------------
// TransactionCommit — message sent to Committers
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug)]
pub struct TransactionCommit {
    pub trans_id: Vec<u8>,
    pub token_id: Vec<u8>,
    pub env_id: String,
    pub proposed_block: Block,
}

// ---------------------------------------------------------------------------
// TransactionPool — per-token ordered queue for leader block proposal
// ---------------------------------------------------------------------------

/// A per-token ordered queue of transaction IDs for deterministic
/// leader block proposal. Sorted by (sequence_number ascending,
/// timestamp ascending) within each sender, and by (token_id,
/// timestamp ascending) across senders.
#[derive(Debug, Default)]
pub struct TransactionPool {
    /// token_id → ordered list of transaction IDs
    pools: HashMap<Vec<u8>, Vec<String>>,
    /// tx_id → (token_id, sequence_number, timestamp, sender) for removal
    index: HashMap<String, (Vec<u8>, usize, i64, Vec<u8>)>,
}

impl TransactionPool {
    pub fn new() -> Self {
        TransactionPool {
            pools: HashMap::new(),
            index: HashMap::new(),
        }
    }

    /// Add a validated transaction to the pool, inserting in
    /// correct order within the token's queue.
    pub fn enqueue(&mut self, tx_id: String, token_id: Vec<u8>,
                   sequence_number: usize, timestamp: i64, sender: Vec<u8>) {
        let entry = (token_id.clone(), sequence_number, timestamp, sender.clone());

        // Insert into index
        self.index.insert(tx_id.clone(), entry.clone());

        // Insert into token pool in sorted position
        let pool = self.pools.entry(token_id).or_insert_with(Vec::new);
        let pos = pool.iter().position(|id| {
            if let Some(existing) = self.index.get(id) {
                // Same sender → sort by sequence_number ascending
                if existing.3 == sender {
                    return existing.1 > sequence_number;
                }
                // Different senders → sort by (sequence_number, timestamp) ascending
                (existing.1, existing.2) > (sequence_number, timestamp)
            } else {
                true
            }
        });
        match pos {
            Some(i) => pool.insert(i, tx_id),
            None => pool.push(tx_id),
        }
    }

    /// Remove a transaction from the pool. Returns the indexed metadata.
    pub fn remove(&mut self, tx_id: &str) -> Option<(Vec<u8>, usize, i64, Vec<u8>)> {
        let entry = self.index.remove(tx_id)?;
        let token_id = entry.0.clone();
        if let Some(pool) = self.pools.get_mut(&token_id) {
            pool.retain(|id| id != tx_id);
            if pool.is_empty() {
                self.pools.remove(&token_id);
            }
        }
        Some(entry)
    }

    /// Return the top n transaction IDs in deterministic order for a given token.
    pub fn peek_top(&self, token_id: &[u8], n: usize) -> Vec<String> {
        self.pools.get(token_id).map_or(vec![], |pool| {
            pool.iter().take(n).cloned().collect()
        })
    }

    /// Drain and return the top n tx_ids for a token (removes them from pool).
    pub fn dequeue_for_leader(&mut self, token_id: &[u8], n: usize) -> Vec<String> {
        let ids = self.peek_top(token_id, n);
        if let Some(pool) = self.pools.get_mut(token_id) {
            let remaining = pool.split_off(n.min(pool.len()));
            if remaining.is_empty() {
                self.pools.remove(token_id);
            } else {
                *pool = remaining;
            }
        }
        ids
    }

    /// Returns the number of pending transactions for a token.
    pub fn len(&self, token_id: &[u8]) -> usize {
        self.pools.get(token_id).map_or(0, |p| p.len())
    }

    /// Returns true if the pool is empty for a token.
    pub fn is_empty(&self, token_id: &[u8]) -> bool {
        self.pools.get(token_id).map_or(true, |p| p.is_empty())
    }

    /// Total transactions across all tokens.
    pub fn total_len(&self) -> usize {
        self.index.len()
    }

    /// Returns an iterator over all known token_ids with pending txs.
    pub fn token_ids(&self) -> std::collections::hash_map::Keys<Vec<u8>, Vec<String>> {
        self.pools.keys()
    }
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- helpers ---

    fn make_test_tx() -> Transaction {
        Transaction {
            id: "tx_test".into(),
            action: "Transfer".into(),
            token_id: vec![1, 2],
            bid: None,
            sequence_number: 1,
            sender: vec![10],
            receiver: vec![20],
            amount: Some(100),
            timestamp: 1000,
            result_hash: vec![],
        }
    }

    fn make_validated_result() -> TransactionValidationResult {
        TransactionValidationResult::valid(
            vec![1, 2, 3],
            TransactionRiskFactor {
                affected_parties: 2,
                amount: 100,
                is_contract: false,
                is_multi_party: false,
            },
        )
    }

    // --- TransactionState lifecycle ---

    #[test]
    fn state_is_pending_new_pending() {
        let state = TransactionState::Pending;
        assert!(state.is_pending());
    }

    #[test]
    fn state_is_pending_validated_is_false() {
        let state = TransactionState::Validated {
            transaction: make_test_tx(),
            validation: make_validated_result(),
        };
        assert!(!state.is_pending());
    }

    #[test]
    fn state_is_pending_committed_is_false() {
        let state = TransactionState::Committed {
            transaction: make_test_tx(),
            block_hash: vec![1],
        };
        assert!(!state.is_pending());
    }

    #[test]
    fn state_is_finalizing_sets_and_clears() {
        let mut state = TransactionState::Pending;
        assert!(!state.is_finalizing());
        state = TransactionState::Finalizing {
            transaction: make_test_tx(),
            finalizer_key: vec![5],
        };
        assert!(state.is_finalizing());
        state = TransactionState::Committed {
            transaction: make_test_tx(),
            block_hash: vec![1],
        };
        assert!(!state.is_finalizing());
    }

    #[test]
    fn state_transaction_returns_none_when_pending() {
        let state = TransactionState::Pending;
        assert!(state.transaction().is_none());
    }

    #[test]
    fn state_transaction_returns_payload_when_validated() {
        let tx = make_test_tx();
        let state = TransactionState::Validated {
            transaction: tx.clone(),
            validation: make_validated_result(),
        };
        let retrieved = state.transaction().expect("should have transaction");
        assert_eq!(retrieved.id, "tx_test");
    }

    #[test]
    fn state_transaction_returns_payload_when_committed() {
        let tx = make_test_tx();
        let state = TransactionState::Committed {
            transaction: tx.clone(),
            block_hash: vec![1],
        };
        let retrieved = state.transaction().expect("should have transaction");
        assert_eq!(retrieved.id, "tx_test");
    }

    #[test]
    fn state_transaction_in_failed_returns_transaction() {
        let tx = make_test_tx();
        let state = TransactionState::Failed {
            transaction: tx.clone(),
            reasons: vec![ValidationFailureReason::InsufficientFunds],
        };
        let retrieved = state.transaction().expect("should have transaction");
        assert_eq!(retrieved.id, "tx_test");
    }

    // --- PendingTransaction acquire ---

    #[test]
    fn acquire_pending_succeeds() {
        let mut pt = PendingTransaction::new("tx1".into(), TransactionState::Pending);
        assert!(pt.acquire().is_ok());
        // Acquire again from Pending — should also succeed
        assert!(pt.acquire().is_ok());
    }

    #[test]
    fn acquire_preloaded_succeeds() {
        let mut pt = PendingTransaction::new(
            "tx1".into(),
            TransactionState::Preloaded { transaction: make_test_tx() },
        );
        assert!(pt.acquire().is_ok());
    }

    #[test]
    fn acquire_executing_succeeds() {
        let mut pt = PendingTransaction::new(
            "tx1".into(),
            TransactionState::Executing { transaction: make_test_tx() },
        );
        assert!(pt.acquire().is_ok());
    }

    #[test]
    fn acquire_failed_returns_err() {
        let mut pt = PendingTransaction::new(
            "tx1".into(),
            TransactionState::Failed {
                transaction: make_test_tx(),
                reasons: vec![],
            },
        );
        assert!(pt.acquire().is_err());
    }

    #[test]
    fn acquire_committed_returns_err() {
        let mut pt = PendingTransaction::new(
            "tx1".into(),
            TransactionState::Committed {
                transaction: make_test_tx(),
                block_hash: vec![1],
            },
        );
        assert!(pt.acquire().is_err());
    }

    // --- PendingTransaction release ---

    #[test]
    fn release_zero_count_pending_does_not_remove() {
        let mut pt = PendingTransaction::new("tx1".into(), TransactionState::Pending);
        // lock_count=0, state=Pending → release returns false (not terminal)
        assert!(!pt.release());
    }

    #[test]
    fn release_nonzero_pending_decrements_to_zero() {
        let mut pt = PendingTransaction::new("tx1".into(), TransactionState::Pending);
        pt.acquire().unwrap(); // lock_count = 1
        assert!(!pt.release()); // lock_count → 0, not terminal → false
        assert!(!pt.release()); // lock_count = 0, not terminal → false
    }

    #[test]
    fn release_failed_zero_count_removes() {
        let mut pt = PendingTransaction::new(
            "tx1".into(),
            TransactionState::Failed {
                transaction: make_test_tx(),
                reasons: vec![],
            },
        );
        assert!(pt.release()); // lock_count=0, Failed → true
    }

    #[test]
    fn release_failed_one_count_removes_after_decrement() {
        let mut pt = PendingTransaction::new(
            "tx1".into(),
            TransactionState::Pending,
        );
        pt.acquire().unwrap(); // lock_count = 1
        // Transition to Failed (acquire was done before transition)
        pt.transition_to_failed(make_test_tx(), vec![]);
        assert!(pt.release()); // lock_count → 0, Failed → true
    }

    #[test]
    fn release_committed_zero_count_removes() {
        let mut pt = PendingTransaction::new(
            "tx1".into(),
            TransactionState::Committed {
                transaction: make_test_tx(),
                block_hash: vec![1],
            },
        );
        assert!(pt.release()); // lock_count=0, Committed → true
    }

    // --- TransactionPool tests ---

    fn make_pool() -> TransactionPool {
        TransactionPool::new()
    }

    #[test]
    fn pool_empty_returns_zero_len() {
        let pool = make_pool();
        assert_eq!(pool.len(&[1, 2, 3]), 0);
        assert!(pool.is_empty(&[1, 2, 3]));
        assert_eq!(pool.total_len(), 0);
    }

    #[test]
    fn pool_enqueue_and_peek_top_returns_single() {
        let mut pool = make_pool();
        pool.enqueue("tx1".into(), vec![1, 2], 1, 1000, vec![10]);
        let ids = pool.peek_top(&[1, 2], 1);
        assert_eq!(ids, vec!["tx1".to_string()]);
    }

    #[test]
    fn pool_enqueue_same_sender_sorted_by_sequence() {
        let mut pool = make_pool();
        pool.enqueue("tx2".into(), vec![1, 2], 2, 1000, vec![10]);
        pool.enqueue("tx1".into(), vec![1, 2], 1, 1000, vec![10]);
        let ids = pool.peek_top(&[1, 2], 2);
        assert_eq!(ids, vec!["tx1".to_string(), "tx2".to_string()]);
    }

    #[test]
    fn pool_enqueue_different_senders_sorted_by_sequence_and_timestamp() {
        let mut pool = make_pool();
        // sender[20] seq=2, ts=2000
        pool.enqueue("tx_c".into(), vec![1, 2], 2, 2000, vec![20]);
        // sender[10] seq=2, ts=1000
        pool.enqueue("tx_a".into(), vec![1, 2], 2, 1000, vec![10]);
        // sender[30] seq=1, ts=1000
        pool.enqueue("tx_b".into(), vec![1, 2], 1, 1000, vec![30]);
        let ids = pool.peek_top(&[1, 2], 3);
        // Order: seq=1 first (tx_b), then seq=2 by timestamp (tx_a ts=1000, tx_c ts=2000)
        assert_eq!(ids, vec!["tx_b".to_string(), "tx_a".to_string(), "tx_c".to_string()]);
    }

    #[test]
    fn pool_remove_not_returned_by_peek_top() {
        let mut pool = make_pool();
        pool.enqueue("tx1".into(), vec![1, 2], 1, 1000, vec![10]);
        pool.enqueue("tx2".into(), vec![1, 2], 2, 1000, vec![20]);
        pool.remove("tx1");
        let ids = pool.peek_top(&[1, 2], 2);
        assert_eq!(ids, vec!["tx2".to_string()]);
    }

    #[test]
    fn pool_dequeue_drains_from_pool() {
        let mut pool = make_pool();
        pool.enqueue("tx1".into(), vec![1, 2], 1, 1000, vec![10]);
        pool.enqueue("tx2".into(), vec![1, 2], 2, 1000, vec![20]);
        let drained = pool.dequeue_for_leader(&[1, 2], 2);
        assert_eq!(drained, vec!["tx1".to_string(), "tx2".to_string()]);
        assert!(pool.is_empty(&[1, 2]));
    }

    #[test]
    fn pool_dequeue_partial_returns_only_available() {
        let mut pool = make_pool();
        pool.enqueue("tx1".into(), vec![1, 2], 1, 1000, vec![10]);
        let drained = pool.dequeue_for_leader(&[1, 2], 5);
        assert_eq!(drained, vec!["tx1".to_string()]);
    }

    #[test]
    fn pool_different_tokens_separate_queues() {
        let mut pool = make_pool();
        pool.enqueue("tx_a1".into(), vec![1], 1, 1000, vec![10]);
        pool.enqueue("tx_b1".into(), vec![2], 1, 1000, vec![10]);
        let a = pool.peek_top(&[1], 1);
        let b = pool.peek_top(&[2], 1);
        assert_eq!(a, vec!["tx_a1".to_string()]);
        assert_eq!(b, vec!["tx_b1".to_string()]);
    }

    #[test]
    fn pool_token_ids_returns_keys() {
        let mut pool = make_pool();
        pool.enqueue("tx1".into(), vec![1], 1, 1000, vec![10]);
        pool.enqueue("tx2".into(), vec![2], 1, 1000, vec![10]);
        let mut keys: Vec<Vec<u8>> = pool.token_ids().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec![vec![1], vec![2]]);
    }
}
