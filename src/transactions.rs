use std::collections::HashMap;
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
        }
    }
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
}
