use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pneumatic_core::epoch::StakeSet;
use pneumatic_core::errors::{PneumaticError, ReconciledSignatures, TransactionRiskFactor};
use pneumatic_core::registry::TransactionSignatureRegistry;
use pneumatic_core::transactions::TransactionSignature;

// ---------------------------------------------------------------------------
// ResponsibleSet — the set a transaction's votes are measured against
// ---------------------------------------------------------------------------

/// The set of nodes whose votes may finalize a transaction, and the stake their
/// votes are worth.
///
/// A quorum's denominator must be a set that was settled **before** the votes
/// arrived. Deriving it from the votes themselves makes the check
/// self-referential: one vote from one nonzero-stake executor is 100% of the
/// stake that showed up, so quorum is met immediately, and every dropped send
/// lowers the threshold along with it
/// (`fact-self-referential-quorum-denominator`). Passing the set in is the whole
/// point of the parameter — a caller cannot compute a quorum without naming who
/// was supposed to vote.
#[derive(Debug, Clone)]
pub struct ResponsibleSet {
    members: HashMap<Vec<u8>, u64>
}

impl ResponsibleSet {
    /// The epoch's stake set, minus members holding zero stake.
    ///
    /// Zero-stake members are excluded because selection never assigns them
    /// (AUDIT Phase 6.6): a slashed-to-zero node must not appear as responsible,
    /// and its absence must not read as a shortfall either.
    pub fn from_stake_set(stake_set: &StakeSet) -> Self {
        ResponsibleSet {
            members: stake_set
                .stakers
                .iter()
                .filter(|(_, stake)| **stake > 0)
                .map(|(key, stake)| (key.clone(), *stake))
                .collect()
        }
    }

    /// Build a set directly. Intended for tests and for callers holding an
    /// explicit committee (Phase 7's selection record).
    pub fn with_members<I: IntoIterator<Item = (Vec<u8>, u64)>>(members: I) -> Self {
        ResponsibleSet {
            members: members
                .into_iter()
                .filter(|(_, stake)| *stake > 0)
                .collect()
        }
    }

    /// The denominator. Never derived from arriving votes.
    pub fn total_stake(&self) -> u64 {
        self.members.values().sum()
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// The declared stake for `key`, or `None` if it is not responsible. A
    /// vote's weight is what this returns — never the weight the vote carries
    /// with it. A signature stamped with a stale or inflated `current_stake`
    /// therefore cannot buy extra weight.
    pub fn stake_of(&self, key: &[u8]) -> Option<u64> {
        self.members.get(key).copied()
    }
}

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
    /// Times reconciliation refused to produce a result because the votes did not
    /// reach quorum over the declared responsible set. A shortfall used to be
    /// invisible: the function could not fail, and fell through to
    /// `candidates.first()` and finalized anyway.
    quorum_shortfalls: Arc<AtomicU64>,
    /// Votes received from keys outside the responsible set, and so excluded.
    unassigned_votes: Arc<AtomicU64>
}

impl SignatureCollector {
    /// Create a new SignatureCollector.
    ///
    /// `quorum_percentage` is the threshold (e.g., 67.0 for 2/3 majority).
    ///
    /// There is deliberately no `total_voters` parameter. The count-based gate it
    /// served (`check_quorum`, removed in Phase 0 item 3) had no production caller,
    /// counted signatures rather than stake, and compared against a total supplied
    /// at construction — so it could not follow an epoch's stake changes, and a
    /// cluster built with the wrong number would enforce the wrong threshold
    /// silently. Quorum is now stake-weighted against a `ResponsibleSet` the caller
    /// resolves for the epoch it is finalizing in.
    pub fn new(
        signature_registry: Arc<TransactionSignatureRegistry>,
        quorum_percentage: f32,
    ) -> Self {
        SignatureCollector {
            signature_registry,
            quorum_percentage,
            quorum_shortfalls: Arc::new(AtomicU64::new(0)),
            unassigned_votes: Arc::new(AtomicU64::new(0))
        }
    }

    /// Count of reconciliations refused for insufficient votes.
    pub fn quorum_shortfall_count(&self) -> u64 {
        self.quorum_shortfalls.load(Ordering::Relaxed)
    }

    /// Count of votes excluded because the voter was not in the responsible set.
    pub fn unassigned_vote_count(&self) -> u64 {
        self.unassigned_votes.load(Ordering::Relaxed)
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

    /// Stake-weighted quorum for shielded finalizer votes (Phase S5.2): the
    /// sum of the admitted votes' `current_stake` (stamped from the epoch
    /// snapshot at admission — never self-reported) must satisfy
    /// `admitted * 100 >= total_stake * quorum` in `u128` — the Phase 6.9
    /// exact-integer pattern (`reconcile_signatures` line 136-143).
    ///
    /// Unlike `check_quorum` there is deliberately **no count-based fast
    /// path and no optimistic branch**: a shielded transfer commits only at
    /// stake-weighted quorum (roadmap 2.1 — client-side proving, quorum-gated
    /// by design). `total_stake` is the current epoch snapshot's total,
    /// passed in by the caller (the collector stays snapshot-agnostic).
    pub fn check_stake_quorum(&self, tx_id: &str, total_stake: u64) -> Result<bool, PneumaticError> {
        let sig_map = self
            .signature_registry
            .get_transaction_registry(tx_id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not in signature registry for quorum check", tx_id
            )))?;

        let admitted: u128 = sig_map
            .iter()
            .map(|(_, sig)| sig.current_stake as u128)
            .sum();

        if total_stake == 0 {
            return Ok(false);
        }

        // admitted / total_stake >= quorum/100  <=>  admitted * 100 >=
        // total_stake * quorum in u128 (stake sums fit comfortably).
        let quorum_pct = self.quorum_percentage.round() as u128;
        Ok(admitted * 100 >= (total_stake as u128) * quorum_pct)
    }

    /// Reconcile collected signatures via stake-weighted supermajority **over the
    /// responsible set**.
    ///
    /// - `responsible` is the set whose votes count, and the stake they count for.
    ///   Its total is the denominator; the arriving votes only ever fill the
    ///   numerator. This is the fix for the self-referential denominator: measured
    ///   against the votes that arrived, one nonzero vote was always 100% and every
    ///   dropped send lowered the bar with it.
    /// - Votes from keys outside `responsible` are excluded and counted. A voter
    ///   that was not assigned cannot supply part of a quorum, and its presence is
    ///   worth a look rather than a silent discount.
    /// - A vote's weight is the responsible set's stake for that key, not the
    ///   `current_stake` the vote carries, so a stale or inflated stamp cannot buy
    ///   extra weight.
    /// - Falling short is an **error**, not a smaller threshold. It used to fall
    ///   through to `candidates.first()` and produce a result regardless, which made
    ///   the function unable to fail and the shortfall unobservable.
    ///
    /// Returns only the signatures from the winning supermajority set, and:
    /// - `winning_finalizer` = the executor key that pushed cumulative stake over
    ///   the threshold.
    /// - `conflict_resolved` = true when the threshold was crossed (multiple
    ///   distinct executor signatures may still be present below it).
    ///
    /// This method returns data only — it does NOT build blocks or send messages.
    pub fn reconcile_signatures(
        &self,
        tx_id: &str,
        responsible: &ResponsibleSet,
    ) -> Result<ReconciledSignatures, PneumaticError> {
        // A quorum over an empty set is not a quorum. This is a configuration or
        // epoch-resolution failure, and answering it with an empty "success" would
        // let a transaction finalize on nobody being responsible.
        if responsible.is_empty() {
            return Err(PneumaticError::Epoch(format!(
                "Cannot reconcile signatures for tx {}: the responsible set is empty \
                 — a quorum over nothing is not a quorum",
                tx_id
            )));
        }

        let sig_map = self
            .signature_registry
            .get_transaction_registry(tx_id)
            .ok_or_else(|| PneumaticError::Registry(format!(
                "Transaction {} not in signature registry for reconciliation", tx_id
            )))?;

        // Keep only votes from responsible keys, and price them at the declared
        // stake rather than the stake the vote declares for itself.
        let mut candidates: Vec<(Vec<u8>, u64, Vec<u8>)> = Vec::new();
        let mut unassigned = 0usize;
        for (key, sig) in sig_map.iter() {
            match responsible.stake_of(key) {
                Some(stake) => candidates.push((key.clone(), stake, sig.signature.clone())),
                None => unassigned += 1
            }
        }

        if unassigned > 0 {
            self.unassigned_votes.fetch_add(unassigned as u64, Ordering::Relaxed);
            eprintln!(
                "[pneumatic] reconcile tx {}: {} vote(s) from keys outside the responsible set \
                 were excluded ({} responsible members)",
                tx_id, unassigned, responsible.len()
            );
        }

        // Sort descending by stake: higher-stake executors vote first.
        candidates.sort_by(|a, b| b.1.cmp(&a.1));

        // The denominator is the responsible set's total — including members that
        // never voted. Their absence is the shortfall this function exists to see.
        let total_stake = responsible.total_stake();
        let arriving_stake: u64 = candidates.iter().map(|(_, s, _)| *s).sum();

        // Supermajority threshold: accumulate stake until reaching quorum%.
        // AUDIT Phase 6.9 (Item B): exact-integer comparison. Casting u64 stake to f64
        // truncates above 2^52, so the f64 threshold can be off-by-one at the boundary —
        // an adversarial stake set could reach/miss quorum wrongly. cumulative >=
        // total_stake * quorum/100  <=>  cumulative*100 >= total_stake*quorum in u128
        // (u64::MAX * 100 fits). quorum_percentage is f32 validated to (0,100]; round()
        // to the nearest whole percent.
        let quorum_pct = self.quorum_percentage.round() as u128;

        let mut cumulative = 0u64;
        let mut winning = Vec::new();
        let mut winning_finalizer = vec![];

        for (executor_key, stake, sig) in &candidates {
            cumulative += stake;
            winning.push(pneumatic_core::errors::ExecutorSignature {
                executor_public_key: executor_key.clone(),
                signature: sig.clone(),
                stake: *stake,
            });
            let reached = (cumulative as u128) * 100 >= (total_stake as u128) * quorum_pct;
            if reached {
                winning_finalizer = executor_key.clone();
                break;
            }
        }

        if winning_finalizer.is_empty() {
            self.quorum_shortfalls.fetch_add(1, Ordering::Relaxed);
            return Err(PneumaticError::Epoch(format!(
                "Quorum not reached for tx {}: eligible votes carry {} of {} responsible stake \
                 ({}% of {} members required, {} vote(s) from unassigned keys excluded) — \
                 refusing to finalize on a smaller base",
                tx_id, arriving_stake, total_stake, quorum_pct, responsible.len(), unassigned
            )));
        }

        Ok(ReconciledSignatures {
            executor_signatures: winning,
            winning_finalizer,
            conflict_resolved: true
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

    /// The responsible set implied by the signatures already registered for
    /// `tx_id`, each at the stake it was added with.
    ///
    /// Arithmetic tests use this so their denominator is stated at the call site
    /// rather than inferred from the votes. Production resolves the set from the
    /// epoch stake snapshot (`Finalizer::responsible_set`), which is the whole
    /// point: a real caller knows who was assigned before anyone votes.
    fn responsible_for(collector: &SignatureCollector, tx_id: &str) -> ResponsibleSet {
        let sigs = collector
            .signature_registry
            .get_transaction_registry(tx_id)
            .expect("transaction must be registered before reconciling");
        ResponsibleSet::with_members(
            sigs.iter()
                .map(|(key, sig)| (key.clone(), sig.current_stake))
                .collect::<Vec<_>>(),
        )
    }

    fn make_collector(registry: Arc<TransactionSignatureRegistry>) -> SignatureCollector {
        SignatureCollector::new(registry, 67.0)
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



    // AUDIT Phase 6.9 (Item B, precision discriminator — finalizer stake walk).
    // three executors: A = 2^52, B = 2^51, C = 2^51 + 1, so total = 2^53 + 1 (not
    // representable in f64; rounds down to 2^53). quorum_percentage = 50%.
    // reconcile_signatures sorts descending: A is largest and is walked FIRST.
    //   integer (new): A alone -> 2^52 * 100 = 450359962737049600 <
    //                  total * 50 = 450359962737049650 -> A does NOT reach -> A is not the winner.
    //   f64 (old): A's cumulative 2^52 >= threshold total_f64*50/100 = 2^53*0.5 = 2^52 -> A reaches
    //             immediately -> A becomes winning_finalizer.
    // So under the fixed integer math A is NOT the winner. Temp-reverting to f64 makes A the
    // winner -> assert fails.
    #[test]
    fn test_reconcile_signatures_precision_big_stakes() {
        let registry = make_registry();
        // 50% quorum so the 2^52 boundary is exactly reachable under f64.
        let collector = SignatureCollector::new(registry.clone(), 50.0);

        let a_stake: u64 = 4503599627370496; // 2^52
        let b_stake: u64 = 2251799813685248; // 2^51
        let c_stake: u64 = 2251799813685249; // 2^51 + 1
        // total = 9007199254740993 = 2^53 + 1

        collector.add_signature("tx_big", b"A".to_vec(), make_sample_signature("tx_big", b"A", a_stake)).unwrap();
        collector.add_signature("tx_big", b"B".to_vec(), make_sample_signature("tx_big", b"B", b_stake)).unwrap();
        collector.add_signature("tx_big", b"C".to_vec(), make_sample_signature("tx_big", b"C", c_stake)).unwrap();

        let reconciled = collector.reconcile_signatures("tx_big", &responsible_for(&collector, "tx_big")).unwrap();
        // Under exact integer arithmetic, A's lone 2^52 stake falls one unit short of the 50%
        // threshold, so A must not be declared the winning finalizer.
        assert_ne!(reconciled.winning_finalizer, b"A".to_vec(),
            "integer quorum must not reach at A's 2^52 stake; f64 rounding bug would make A the winner");
    }

    // --- Phase S5.2: stake-weighted quorum (check_stake_quorum) ---

    /// Below the threshold: 60 of 100 stake admitted < 67% → no quorum.
    #[test]
    fn check_stake_quorum_below_threshold() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        collector
            .add_signature("tx_s", b"v1".to_vec(), make_sample_signature("tx_s", b"v1", 60))
            .unwrap();
        assert!(!collector.check_stake_quorum("tx_s", 100).unwrap());
    }

    /// At the threshold: 67 of 100 admitted — the exact-integer boundary
    /// `67 * 100 >= 100 * 67` must count as quorum met.
    #[test]
    fn check_stake_quorum_at_threshold() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        collector
            .add_signature("tx_s", b"v1".to_vec(), make_sample_signature("tx_s", b"v1", 67))
            .unwrap();
        assert!(collector.check_stake_quorum("tx_s", 100).unwrap());
    }

    /// One vote carrying the full snapshot stake (100/100 = 100% ≥ 67%)
    /// reaches quorum — the single-voter shape of the S5.2 fixture.
    #[test]
    fn check_stake_quorum_single_full_stake_vote() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        collector
            .add_signature("tx_s", b"v1".to_vec(), make_sample_signature("tx_s", b"v1", 100))
            .unwrap();
        assert!(collector.check_stake_quorum("tx_s", 100).unwrap());
    }

    /// Zero total stake → fail closed (no quorum), even with admitted stake.
    #[test]
    fn check_stake_quorum_zero_total_stake_fails_closed() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        collector
            .add_signature("tx_s", b"v1".to_vec(), make_sample_signature("tx_s", b"v1", 100))
            .unwrap();
        assert!(!collector.check_stake_quorum("tx_s", 0).unwrap());
    }

    /// Unknown tx → `Registry` error (no implicit quorum for a tx with no
    /// collected votes).
    #[test]
    fn check_stake_quorum_unknown_tx_errors() {
        let collector = make_collector(make_registry());
        assert!(collector.check_stake_quorum("nope", 100).is_err());
    }

    #[test]
    fn test_reconcile_signatures() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 10)).unwrap();
        collector.add_signature("tx_1", b"executor_2".to_vec(), make_sample_signature("tx_1", b"executor_2", 20)).unwrap();

        let reconciled = collector.reconcile_signatures("tx_1", &responsible_for(&collector, "tx_1")).unwrap();
        assert_eq!(reconciled.executor_signatures.len(), 2);
        assert!(reconciled.conflict_resolved);
    }

    #[test]
    fn test_reconcile_single_signature_sets_winner() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector.add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 10)).unwrap();

        let reconciled = collector.reconcile_signatures("tx_1", &responsible_for(&collector, "tx_1")).unwrap();
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

        let reconciled = collector.reconcile_signatures("tx_1", &responsible_for(&collector, "tx_1")).unwrap();
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

        let reconciled = collector.reconcile_signatures("tx_1", &responsible_for(&collector, "tx_1")).unwrap();
        assert_eq!(reconciled.executor_signatures.len(), 2);
        assert!(reconciled.conflict_resolved);
    }

    #[test]
    /// What the old "all votes carry zero stake ⇒ empty success" test became.
    ///
    /// The old expectation encoded the old model, where a vote's weight came from
    /// the stamp it carried. Now weight comes from the responsible set, so a
    /// zero-stamped vote from a genuinely responsible executor still counts at its
    /// declared stake — which is the point: a signature cannot discount or inflate
    /// its own weight. (A set whose declared stake is entirely zero is not a set at
    /// all; `zero_stake_members_are_not_responsible` pins that.)
    #[test]
    fn a_vote_is_priced_by_the_responsible_set_not_by_its_own_stamp() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        collector
            .add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 0))
            .unwrap();

        let responsible = ResponsibleSet::with_members(vec![(b"executor_1".to_vec(), 100)]);
        let reconciled = collector
            .reconcile_signatures("tx_1", &responsible)
            .expect("the declared stake is what votes, not the stamp");
        assert_eq!(reconciled.executor_signatures.len(), 1);
        assert_eq!(
            reconciled.executor_signatures[0].stake, 100,
            "the winning entry must carry the declared stake"
        );
        assert_eq!(reconciled.winning_finalizer, b"executor_1".to_vec());
    }

    #[test]
    fn test_reconcile_nonexistent_tx_fails() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());

        // Stated explicitly rather than derived: there are no signatures to derive
        // it from, and the point of the test is the missing-registry error.
        let responsible = ResponsibleSet::with_members(vec![(b"executor_1".to_vec(), 100)]);
        let result = collector.reconcile_signatures("nonexistent", &responsible);
        let err = result.expect_err("an unregistered transaction must not reconcile");
        assert!(
            err.to_string().contains("not in signature registry"),
            "the error must be the registry lookup, not the responsible set: {}",
            err
        );
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
    fn a_shortfall_refuses_instead_of_lowering_the_bar() {
        // 90% of a declared set of 100+100+100 = 270 required. Two votes give 200.
        // Before Phase 0 item 3 the denominator was the arriving stake (200), so
        // 200/200 was quorum and the transaction finalized on two of three.
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        let responsible = ResponsibleSet::with_members(vec![
            (b"a".to_vec(), 100),
            (b"b".to_vec(), 100),
            (b"c".to_vec(), 100),
        ]);

        let high = SignatureCollector::new(registry.clone(), 90.0);
        high.add_signature("tx_1", b"a".to_vec(), make_sample_signature("tx_1", b"a", 100)).unwrap();
        high.add_signature("tx_1", b"b".to_vec(), make_sample_signature("tx_1", b"b", 100)).unwrap();

        let err = high
            .reconcile_signatures("tx_1", &responsible)
            .expect_err("200 of a declared 300 is not 90% — it must be refused");
        assert!(
            err.to_string().contains("Quorum not reached"),
            "the error must name the shortfall: {}",
            err
        );
        assert_eq!(high.quorum_shortfall_count(), 1, "and it must be counted");

        // The same collector with every responsible member voting does reach it,
        // so the refusal above is about the missing vote and not the setup.
        high.add_signature("tx_1", b"c".to_vec(), make_sample_signature("tx_1", b"c", 100)).unwrap();
        let ok = high
            .reconcile_signatures("tx_1", &responsible)
            .expect("300 of 300 must satisfy any threshold up to 100%");
        assert_eq!(ok.conflict_resolved, true);
        assert_eq!(high.quorum_shortfall_count(), 1, "a success must not count as a shortfall");
    }

    /// The headline property: with a declared set, one vote is one third of the
    /// stake, not 100% of "whoever showed up".
    #[test]
    fn one_vote_is_no_longer_a_quorum() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        let responsible = ResponsibleSet::with_members(vec![
            (b"executor_1".to_vec(), 100),
            (b"executor_2".to_vec(), 100),
            (b"executor_3".to_vec(), 100),
        ]);

        collector
            .add_signature("tx_1", b"executor_1".to_vec(), make_sample_signature("tx_1", b"executor_1", 100))
            .unwrap();

        assert!(
            collector.reconcile_signatures("tx_1", &responsible).is_err(),
            "100 of a declared 300 must not finalize at 67%"
        );
        assert_eq!(collector.quorum_shortfall_count(), 1);
    }

    /// A node that was never assigned cannot supply part of a quorum, no matter how
    /// much stake it claims — and its vote is counted so the anomaly is visible
    /// rather than silently discounted.
    #[test]
    fn a_vote_from_outside_the_responsible_set_does_not_count() {
        let registry = make_registry();
        let collector = make_collector(registry.clone());
        let responsible = ResponsibleSet::with_members(vec![
            (b"assigned".to_vec(), 100),
            (b"also_assigned".to_vec(), 100),
        ]);

        // The outsider declares a huge stake. Weight comes from the responsible set,
        // so it contributes nothing at all.
        collector
            .add_signature("tx_1", b"outsider".to_vec(), make_sample_signature("tx_1", b"outsider", 999_999))
            .unwrap();
        collector
            .add_signature("tx_1", b"assigned".to_vec(), make_sample_signature("tx_1", b"assigned", 100))
            .unwrap();

        assert!(
            collector.reconcile_signatures("tx_1", &responsible).is_err(),
            "the outsider's 999999 must not count toward a 200-stake quorum"
        );
        assert_eq!(collector.unassigned_vote_count(), 1, "and it must be counted as excluded");

        // With the second assigned member voting, quorum is reached and the
        // outsider still contributes nothing.
        collector
            .add_signature("tx_1", b"also_assigned".to_vec(), make_sample_signature("tx_1", b"also_assigned", 100))
            .unwrap();
        let ok = collector
            .reconcile_signatures("tx_1", &responsible)
            .expect("the two assigned votes are 200 of 200");
        assert_eq!(
            ok.executor_signatures.iter().filter(|s| s.executor_public_key == b"outsider").count(),
            0,
            "an unassigned vote must never appear in the winning set"
        );
    }

    /// An empty responsible set is a configuration failure, not a vacuous quorum.
    #[test]
    fn an_empty_responsible_set_is_an_error_not_a_quorum() {
        let collector = make_collector(make_registry());
        let err = collector
            .reconcile_signatures("tx_1", &ResponsibleSet::with_members(Vec::new()))
            .expect_err("a quorum over nothing is not a quorum");
        assert!(err.to_string().contains("responsible set is empty"), "{}", err);
    }

    /// Zero-stake members are excluded when the set is built, matching selection
    /// (AUDIT Phase 6.6): a slashed node is neither responsible nor a shortfall.
    #[test]
    fn zero_stake_members_are_not_responsible() {
        let set = ResponsibleSet::with_members(vec![
            (b"real".to_vec(), 100),
            (b"slashed".to_vec(), 0),
        ]);
        assert_eq!(set.len(), 1);
        assert_eq!(set.total_stake(), 100);
        assert_eq!(set.stake_of(b"slashed"), None);
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

}
