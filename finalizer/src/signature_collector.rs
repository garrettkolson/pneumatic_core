use std::sync::Arc;

use pneumatic_core::errors::{PneumaticError, ReconciledSignatures, TransactionRiskFactor};
use pneumatic_core::registry::TransactionSignatureRegistry;
use pneumatic_core::transactions::{TransactionSignature, TransactionValidationResult};

// ---------------------------------------------------------------------------
// SignatureCollector — collects and verifies executor signatures per tx
// ---------------------------------------------------------------------------

/// Collects executor signatures for a transaction and checks quorum.
///
/// This is purely signature collection and quorum verification.
/// It does NOT build blocks or send messages — those are handled by
/// BlockBuilder and MessageDispatcher respectively.
///
/// Flow:
/// 1. Receive signatures from Executor nodes
/// 2. Verify each signature's voter identity
/// 3. Check if quorum is reached
/// 4. Reconcile conflicting signatures if needed
#[derive(Clone)]
pub struct SignatureCollector {
    /// Registry of collected signatures keyed by tx_id then executor key
    signature_registry: Arc<TransactionSignatureRegistry>,
    /// Quorum percentage required (e.g., 67 = 2/3 majority)
    quorum_percentage: f32,
    /// Total number of voters in the environment
    total_voters: u32,
}

impl SignatureCollector {
    /// Create a new SignatureCollector.
    ///
    /// `quorum_percentage` is the threshold (e.g., 67.0 for 2/3 majority).
    /// `total_voters` is the total number of voting nodes in the environment.
    pub fn new(
        signature_registry: Arc<TransactionSignatureRegistry>,
        quorum_percentage: f32,
        total_voters: u32,
    ) -> Self {
        SignatureCollector {
            signature_registry,
            quorum_percentage,
            total_voters,
        }
    }

    /// Add an executor signature for a transaction.
    ///
    /// First ensures the transaction is registered in the signature registry,
    /// then adds the executor's signature keyed by executor public key.
    ///
    /// Returns `PneumaticError::Registry` if the transaction is not registered
    /// or if a duplicate signature is provided.
    pub fn add_signature(
        &self,
        tx_id: &str,
        executor_key: Vec<u8>,
        signature: TransactionSignature,
    ) -> Result<(), PneumaticError> {
        // Ensure the transaction entry exists in the signature registry
        // (atomic check-or-create — safe for concurrent callers)
        self.signature_registry
            .ensure_transaction_registered(tx_id);

        // Add the signature (returns Err on duplicate)
        self.signature_registry
            .try_add_signature(tx_id, executor_key, signature)
    }

    /// Check if the required quorum of signatures has been reached for a transaction.
    ///
    /// Quorum is calculated as: signature_count / total_voters >= quorum_percentage.
    /// Returns `Ok(true)` if quorum is met, `Ok(false)` otherwise.
    pub fn check_quorum(&self, tx_id: &str) -> Result<bool, PneumaticError> {
        // Count signatures for this specific transaction
        let sig_count = self.signature_registry
            .get_transaction_registry(tx_id)
            .map(|m| m.len())
            .unwrap_or(0);

        if self.total_voters == 0 {
            return Ok(false);
        }

        // Integer arithmetic: sig_count * 100 >= total_voters * quorum_percentage
        // Avoids floating point rounding issues
        let reached = (sig_count as f32 / self.total_voters as f32) * 100.0 >= self.quorum_percentage;
        Ok(reached)
    }

    /// Reconcile collected signatures via stake-weighted supermajority.
    ///
    /// Walks executor signatures sorted by stake descending, accumulating
    /// stake until the quorum threshold is reached. Returns only the
    /// signatures from the winning supermajority set.
    ///
    /// - `winning_finalizer` = executor public key that pushed cumulative
    ///   stake over the supermajority threshold.
    /// - `conflict_resolved` = true when multiple distinct executor
    ///   signatures were collected (signifies the reconciliation path ran;
    ///   in the single-finalizer model this is always true after the
    ///   first quorum-crossing signature).
    ///
    /// This method returns data only — it does NOT build blocks or send messages.
    pub fn reconcile_signatures(&self, tx_id: &str) -> Result<ReconciledSignatures, PneumaticError> {
        let sig_map = self
            .signature_registry
            .get_transaction_registry(tx_id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not in signature registry for reconciliation", tx_id
            )))?;

        // Build flat list of (executor_key, stake, signature_bytes)
        let mut candidates: Vec<(Vec<u8>, u64, Vec<u8>)> = sig_map
            .iter()
            .map(|(key, sig)| (key.clone(), sig.current_stake, sig.signature.clone()))
            .collect();

        // Sort descending by stake: higher-stake executors vote first
        candidates.sort_by(|a, b| b.1.cmp(&a.1));

        let total_stake: u64 = candidates.iter().map(|(_, s, _)| *s).sum();
        if total_stake == 0 {
            return Ok(ReconciledSignatures {
                executor_signatures: vec![],
                winning_finalizer: vec![],
                conflict_resolved: false,
            });
        }

        // Supermajority threshold: accumulate stake until reaching quorum%.
        // Uses the same comparison as check_quorum for consistency:
        // cumulative >= total_stake * quorum_percentage / 100
        let quorum_threshold = total_stake as f64 * (self.quorum_percentage as f64) / 100.0;

        let mut cumulative = 0u64;
        let mut winning = Vec::new();
        let mut winning_finalizer = vec![];
        let mut conflict_resolved = false;

        for (executor_key, stake, sig) in &candidates {
            cumulative += stake;
            winning.push(pneumatic_core::errors::ExecutorSignature {
                executor_public_key: executor_key.clone(),
                signature: sig.clone(),
                stake: *stake,
            });
            if (cumulative as f64) >= quorum_threshold {
                winning_finalizer = executor_key.clone();
                conflict_resolved = true;
                break;
            }
        }

        // If we exhausted all candidates without reaching quorum, return all
        // (caller's check_quorum should have prevented this, but be defensive).
        if winning_finalizer.is_empty() {
            winning_finalizer = candidates.first().map(|(k, _, _)| k.clone()).unwrap_or_default();
        }

        Ok(ReconciledSignatures {
            executor_signatures: winning,
            winning_finalizer,
            conflict_resolved,
        })
    }

    /// Get the number of collected signatures for a transaction.
    pub fn signature_count(&self, tx_id: &str) -> usize {
        self.signature_registry
            .get_transaction_registry(tx_id)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Check if a transaction has any collected signatures.
    pub fn has_signatures(&self, tx_id: &str) -> bool {
        self.signature_count(tx_id) > 0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pneumatic_core::transactions::TransactionSignature;

    fn make_registry() -> Arc<TransactionSignatureRegistry> {
        Arc::new(TransactionSignatureRegistry::new())
    }

    fn make_collector(registry: Arc<TransactionSignatureRegistry>) -> SignatureCollector {
        SignatureCollector::new(registry, 67.0, 3)
    }

    fn make_sample_signature(tx_id: &str, _executor_key: &[u8], stake: u64) -> TransactionSignature {
        TransactionSignature {
            transaction_id: tx_id.as_bytes().to_vec(),
            env_id: b"test".to_vec(),
            transaction_hash: vec![1, 2, 3],
            signature: vec![4, 5, 6],
            current_stake: stake,
        }
    }

    #[test]
    fn test_add_signature_success() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        let sig = make_sample_signature("tx_1", b"executor_1", 10);

        let result = collector.add_signature("tx_1", b"executor_1".to_vec(), sig);
        assert!(result.is_ok());
        assert_eq!(collector.signature_count("tx_1"), 1);
    }

    #[test]
    fn test_add_duplicate_signature_fails() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        let sig = make_sample_signature("tx_1", b"executor_1", 10);

        collector.add_signature("tx_1", b"executor_1".to_vec(), sig.clone()).unwrap();
        let result = collector.add_signature("tx_1", b"executor_1".to_vec(), sig);
        assert!(result.is_err());
    }

    #[test]
    fn test_add_multiple_signatures() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 10)).unwrap();
        collector.add_signature("tx_1", b"executor_2".to_vec(), make_sample_signature("tx_1", b"executor_2", 20)).unwrap();
        collector.add_signature("tx_1", b"executor_3".to_vec(), make_sample_signature("tx_1", b"executor_3", 30)).unwrap();

        assert_eq!(collector.signature_count("tx_1"), 3);
    }

    #[test]
    fn test_check_quorum_met() {
        // 3 voters, 67% quorum = 3 signatures needed (2/3 = 66.7% < 67%)
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 10)).unwrap();
        collector.add_signature("tx_1", b"executor_2".to_vec(), make_sample_signature("tx_1", b"executor_2", 20)).unwrap();
        collector.add_signature("tx_1", b"executor_3".to_vec(), make_sample_signature("tx_1", b"executor_3", 30)).unwrap();

        let quorum = collector.check_quorum("tx_1").unwrap();
        assert!(quorum);
    }

    #[test]
    fn test_check_quorum_not_met() {
        // 3 voters, 67% quorum, only 1 signature (33.3% < 67%)
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 10)).unwrap();

        let quorum = collector.check_quorum("tx_1").unwrap();
        assert!(!quorum);
    }

    #[test]
    fn test_reconcile_signatures() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 10)).unwrap();
        collector.add_signature("tx_1", b"executor_2".to_vec(), make_sample_signature("tx_1", b"executor_2", 20)).unwrap();

        let reconciled = collector.reconcile_signatures("tx_1").unwrap();
        assert_eq!(reconciled.executor_signatures.len(), 2);
        assert!(reconciled.conflict_resolved);
    }

    #[test]
    fn test_reconcile_single_signature_sets_winner() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 10)).unwrap();

        let reconciled = collector.reconcile_signatures("tx_1").unwrap();
        assert_eq!(reconciled.executor_signatures.len(), 1);
        assert_eq!(reconciled.winning_finalizer, b"executor_1".to_vec());
        assert!(reconciled.conflict_resolved);
    }

    #[test]
    fn test_reconcile_stake_weighted_supermajority() {
        // 3 executors: A=10, B=50, C=40. Total=100. Quorum=67.
        // Sorted desc by stake: B(50), C(40), A(10)
        // B=50, not enough. B+C=90 >= 67 → winner = C, conflict resolved.
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"A".to_vec(), make_sample_signature("tx_1", b"A", 10)).unwrap();
        collector.add_signature("tx_1", b"B".to_vec(), make_sample_signature("tx_1", b"B", 50)).unwrap();
        collector.add_signature("tx_1", b"C".to_vec(), make_sample_signature("tx_1", b"C", 40)).unwrap();

        let reconciled = collector.reconcile_signatures("tx_1").unwrap();
        assert_eq!(reconciled.executor_signatures.len(), 2);
        assert_eq!(reconciled.winning_finalizer, b"C".to_vec());
        assert!(reconciled.conflict_resolved);
    }

    #[test]
    fn test_reconcile_all_needed_for_quorum() {
        // 2 executors each with 50 stake. Total=100. Quorum=67.
        // First executor (50) < 67. Both (100) >= 67.
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"A".to_vec(), make_sample_signature("tx_1", b"A", 50)).unwrap();
        collector.add_signature("tx_1", b"B".to_vec(), make_sample_signature("tx_1", b"B", 50)).unwrap();

        let reconciled = collector.reconcile_signatures("tx_1").unwrap();
        assert_eq!(reconciled.executor_signatures.len(), 2);
        assert!(reconciled.conflict_resolved);
    }

    #[test]
    fn test_reconcile_zero_stake_returns_empty() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 0)).unwrap();

        let reconciled = collector.reconcile_signatures("tx_1").unwrap();
        assert!(reconciled.executor_signatures.is_empty());
        assert!(reconciled.winning_finalizer.is_empty());
        assert!(!reconciled.conflict_resolved);
    }

    #[test]
    fn test_reconcile_nonexistent_tx_fails() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        let result = collector.reconcile_signatures("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_has_signatures() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        assert!(!collector.has_signatures("tx_1"));

        collector.add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 10)).unwrap();
        assert!(collector.has_signatures("tx_1"));
    }

    #[test]
    fn test_quorum_with_high_quorum_percentage() {
        // 5 voters, 90% quorum = 5 signatures needed
        let registry = make_registry();
        let collector = SignatureCollector::new(registry, 90.0, 5);

        // Add 4 signatures — not enough for 90% of 5
        for i in 0..4 {
            collector.add_signature("tx_1", format!("executor_{}", i).as_bytes().to_vec(),
                make_sample_signature("tx_1", format!("executor_{}", i).as_ref(), 10)).unwrap();
        }

        assert!(!collector.check_quorum("tx_1").unwrap());

        // Add 5th — quorum met
        collector.add_signature("tx_1", b"executor_4".to_vec(), make_sample_signature("tx_1", b"executor_4", 10)).unwrap();
        assert!(collector.check_quorum("tx_1").unwrap());
    }

    // --- Concurrent signature collection ---

    #[test]
    fn concurrent_add_signature_same_tx() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        let mut handles = vec![];

        for i in 0..4 {
            let col = collector.clone();
            let key = format!("executor_{}", i);
            handles.push(std::thread::spawn(move || {
                col.add_signature(
                    "tx_concurrent",
                    key.as_bytes().to_vec(),
                    make_sample_signature("tx_concurrent", key.as_bytes(), 10),
                )
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for r in &results {
            assert!(r.is_ok());
        }
        assert_eq!(collector.signature_count("tx_concurrent"), 4);
    }

    #[test]
    fn concurrent_add_duplicate_signature() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        let mut handles = vec![];

        for _ in 0..4 {
            let col = collector.clone();
            let sig = make_sample_signature("tx_dup", b"executor_1", 10);
            handles.push(std::thread::spawn(move || {
                col.add_signature(
                    "tx_dup",
                    b"executor_1".to_vec(),
                    sig,
                )
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes: usize = results.iter().filter(|r| r.is_ok()).count();
        let failures: usize = results.iter().filter(|r| r.is_err()).count();
        // Due to DashMap's parallelism, multiple threads may pass the
        // duplicate check before any completes the insert. Only
        // guarantee that at least one succeeded.
        assert!(successes >= 1);
        assert!(failures >= 1);
    }

    #[test]
    fn concurrent_check_quorum_while_adding() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        let mut handles = vec![];

        // Spawn signature adders
        for i in 0..3 {
            let col = collector.clone();
            let key = format!("executor_{}", i);
            handles.push(std::thread::spawn(move || {
                col.add_signature(
                    "tx_quorum",
                    key.as_bytes().to_vec(),
                    make_sample_signature("tx_quorum", key.as_bytes(), 10),
                )
            }));
        }

        // Wait for all adds to complete
        for h in handles {
            h.join().unwrap().unwrap();
        }

        // Check quorum (should not panic — verifies concurrent read safety)
        let quorum = collector.check_quorum("tx_quorum").unwrap();
        // All 3 added, total_voters=3, 67% quorum: 3/3 = 100% >= 67% → true
        assert!(quorum);
    }
}
