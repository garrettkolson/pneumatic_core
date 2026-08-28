use std::sync::Arc;

use dashmap::DashMap;
use rand::{Rng, SeedableRng};

use pneumatic_core::crypto::HashProvider;
use pneumatic_core::data::DataProvider;
use pneumatic_core::epoch::{CandidateRegistry, Conflict, ConflictResolution, EpochReconciliation, IEpochLeaderSelector, IEpochReconciler, IStakingManager, LEADER_DOMAIN, StakeSet, StakingOp, resolve_block_conflict};
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::logging::Logger;

// ---------------------------------------------------------------------------
// StakeStore — concrete stake state backed by DashMap
// ---------------------------------------------------------------------------

/// Concurrent stake storage that replaces the no-op StubStakingManager.
/// Backed by a DashMap for lock-free concurrent access.
pub struct StakeStore {
    stakes: Arc<DashMap<Vec<u8>, u64>>,
}

impl StakeStore {
    pub fn new() -> Self {
        StakeStore {
            stakes: Arc::new(DashMap::new()),
        }
    }

    /// Add or update a staker with their stake amount.
    pub fn add_staker(&self, key: Vec<u8>, stake: u64) {
        self.stakes.insert(key, stake);
    }

    /// Remove a staker by public key.
    pub fn remove_staker(&self, key: &[u8]) {
        self.stakes.remove(key);
    }

    /// Slash a staker's stake by the given amount.
    pub fn slash(&self, key: &[u8], amount: u64) {
        if let Some(mut entry) = self.stakes.get_mut(key) {
            let new_stake = entry.saturating_sub(amount);
            *entry = new_stake;
        }
    }

    /// Reward a staker with additional stake.
    pub fn reward(&self, key: &[u8], amount: u64) {
        if let Some(mut entry) = self.stakes.get_mut(key) {
            *entry += amount;
        }
    }

    /// Get the current stake for a public key.
    pub fn get_stake(&self, key: &[u8]) -> u64 {
        self.stakes.get(key).map(|e| *e.value()).unwrap_or(0)
    }

    /// Iterate over all stakers.
    pub fn iter(&self) -> impl Iterator<Item = (Vec<u8>, u64)> + '_ {
        self.stakes.iter().map(|e| (e.key().clone(), *e.value()))
    }

    /// Convert to a StakeSet for leader selection.
    pub fn to_stake_set(&self) -> StakeSet {
        StakeSet {
            stakers: self.stakes.iter().map(|e| (e.key().clone(), *e.value())).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// StakingManager — applies reconciliation ops to concrete storage
// ---------------------------------------------------------------------------

/// Applies staking operations from epoch reconciliation to the `StakeStore`.
/// Replaces the no-op `StubStakingManager` from core.
pub struct StakingManager {
    stake_store: Arc<StakeStore>,
    logger: Arc<dyn Logger>,
}

impl StakingManager {
    pub fn new(stake_store: Arc<StakeStore>, logger: Arc<dyn Logger>) -> Self {
        StakingManager { stake_store, logger }
    }
}

impl IStakingManager for StakingManager {
    fn apply_ops(&self, ops: &EpochReconciliation) -> Result<(), PneumaticError> {
        // A single pass over both lists applies each op exactly once. The
        // previous version ran two loops that *both* handled `Slash`, so a
        // slash landing in `reward_ops` would have deducted the stake twice.
        let op_count = ops.slashing_ops.len() + ops.reward_ops.len();

        for op in ops.slashing_ops.iter().chain(ops.reward_ops.iter()) {
            match op {
                StakingOp::Slash(key, amount) => {
                    self.stake_store.slash(key, *amount);
                }
                StakingOp::Reward(key, amount) => {
                    self.stake_store.reward(key, *amount);
                }
                StakingOp::AddStaker(key, stake) => {
                    self.stake_store.add_staker(key.clone(), *stake);
                }
                StakingOp::RemoveStaker(key) => {
                    self.stake_store.remove_staker(key);
                }
            }
        }

        if op_count > 0 {
            self.logger.log(format!("Applied {} staking ops during epoch reconciliation", op_count));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EpochReconciler — examines chain state at epoch boundaries
// ---------------------------------------------------------------------------

/// Examines chain state and returns staking/conflict operations to apply.
/// Uses CandidateRegistry for same-chain conflict detection and StakeStore
/// for real stake resolution (Phase 2 of Protocol Rearchitecture).
pub struct EpochReconciler {
    stake_store: Arc<StakeStore>,
    candidate_registry: Arc<CandidateRegistry>,
    data_provider: Arc<dyn DataProvider>,
    env_id: String,
    token_ids: Vec<Vec<u8>>,
    /// Fraction of a slashed proposer's stake to remove on a resolved
    /// same-proposer conflict (1.0 = full stake). Set from the committer's
    /// `CostModel.slash_fraction` so the penalty is configurable via env spec.
    slash_fraction: f64,
}

impl EpochReconciler {
    pub fn new(
        stake_store: Arc<StakeStore>,
        candidate_registry: Arc<CandidateRegistry>,
        data_provider: Arc<dyn DataProvider>,
        env_id: String,
        token_ids: Vec<Vec<u8>>,
        slash_fraction: f64,
    ) -> Self {
        EpochReconciler {
            stake_store,
            candidate_registry,
            data_provider,
            env_id,
            token_ids,
            slash_fraction,
        }
    }

    /// Run epoch reconciliation — detect misshapen chains and
    /// same-chain fork conflicts via CandidateRegistry.
    ///
    /// For same-chain detection: a conflict exists when the CandidateRegistry
    /// holds 2+ candidates at the same `(token_id, previous_hash)` — meaning
    /// two proposers built on the same parent block.
    fn reconcile_internal(&self) -> EpochReconciliation {
        // Build a StakeSet from StakeStore for conflict resolution
        let mut stake_set = StakeSet {
            stakers: self.stake_store.iter()
                .map(|(key, stake)| (key, stake))
                .collect(),
        };

        let mut reconciliation = EpochReconciliation::default();

        // Load each token and check chain validity / conflicts
        for token_id in &self.token_ids {
            let token = match self.data_provider.get_token(token_id, &self.env_id) {
                Ok(token) => token,
                Err(_) => continue, // token not found, skip
            };

            // Check chain validity
            let chain_state = token.blockchain.get_current_chain_state();
            if !chain_state.is_valid {
                // AUDIT Phase 5.2 / H2: give `misshapen_tokens` a real economic side
                // effect instead of being a dead accumulator. An invalid chain means its
                // tip proposer built something the network rejects, so slash the tip
                // proposer's stake (the same amount formula used for same-proposer
                // double-signs, Phase 5.1). The token id is still recorded in
                // `misshapen_tokens` as an informational record; the slash is the
                // remediation.
                reconciliation.misshapen_tokens.push(token_id.clone());
                if let Some(tip_block) = token.blockchain.last_block() {
                    let amount = (self.stake_store.get_stake(&tip_block.proposer_key) as f64
                        * self.slash_fraction)
                        .round()
                        .min(u64::MAX as f64) as u64;
                    if amount > 0 {
                        reconciliation.slashing_ops.push(StakingOp::Slash(
                            tip_block.proposer_key.clone(),
                            amount,
                        ));
                    }
                }
                continue;
            }

            // Check CandidateRegistry for same-chain conflicts at the tip
            let tip_hash = chain_state.last_hash_in;
            let candidate_count = self.candidate_registry.candidate_count(token_id, &tip_hash);
            if candidate_count >= 2 {
                let candidates = self.candidate_registry.get_candidates(token_id, &tip_hash);
                for pair in candidates.windows(2) {
                    let (block_a, proposer_a) = &pair[0];
                    let (block_b, proposer_b) = &pair[1];

                    let stake_a = self.stake_store.get_stake(&proposer_a);
                    let stake_b = self.stake_store.get_stake(&proposer_b);

                    stake_set.stakers.insert(proposer_a.clone(), stake_a);
                    stake_set.stakers.insert(proposer_b.clone(), stake_b);

                    reconciliation.finalization_conflicts.push(Conflict {
                        block_a: block_a.current_hash.clone(),
                        block_b: block_b.current_hash.clone(),
                        stake_a,
                        stake_b,
                    });

                    // AUDIT Phase 5.1 / H1: resolve the conflict and actually
                    // slash a same-proposer double-sign. `resolve_block_conflict`
                    // only branches to `SameProposerSlash` when stakes are equal
                    // and the proposers match, so this is exactly the
                    // protocol-violation case. Slash the full remaining stake
                    // (times the configured fraction); a proposer already slashed
                    // to 0 at commit time is re-slashed to a no-op.
                    if let Ok(ConflictResolution::SameProposerSlash(_, slashed_key)) =
                        resolve_block_conflict(
                            &block_a.current_hash,
                            &block_b.current_hash,
                            &proposer_a,
                            &proposer_b,
                            &stake_set,
                        )
                    {
                        let amount = (self.stake_store.get_stake(&slashed_key) as f64
                            * self.slash_fraction)
                            .round()
                            .min(u64::MAX as f64) as u64;
                        if amount > 0 {
                            reconciliation.slashing_ops.push(StakingOp::Slash(slashed_key, amount));
                        }
                    }
                }
            }
        }

        reconciliation
    }
}

impl IEpochReconciler for EpochReconciler {
    fn reconcile(&self) -> EpochReconciliation {
        self.reconcile_internal()
    }
}

// ---------------------------------------------------------------------------
// LeaderSelector — stake-weighted random leader selection
// ---------------------------------------------------------------------------

/// Selects the block leader for an epoch using stake-weighted random
/// selection. Replaces `StubLeaderSelector` from core.
pub struct LeaderSelector {
    hash_provider: Arc<dyn HashProvider>,
}

impl LeaderSelector {
    pub fn new(hash_provider: Arc<dyn HashProvider>) -> Self {
        LeaderSelector { hash_provider }
    }

    /// Select leader(s) from the current stake set using
    /// stake-weighted deterministic selection, bound to the mined chain tip
    /// (Phase 5.3 / AUDIT H3).
    fn select_internal(&self, stakers: &StakeSet, epoch_number: u64, prev_block_hash: &[u8]) -> Vec<u8> {
        let total = stakers.total_stake();
        if total == 0 {
            return vec![];
        }

        // Domain-separated seed bound to the mined tip:
        // SHA-256(LEADER_DOMAIN ‖ epoch_number ‖ prev_block_hash ‖ [])
        let mut input = Vec::with_capacity(1 + 8 + prev_block_hash.len());
        input.push(LEADER_DOMAIN);
        input.extend_from_slice(&epoch_number.to_be_bytes());
        input.extend_from_slice(prev_block_hash);
        let seed = self.hash_provider.hash(&input);
        let mut rng = rand::rngs::StdRng::from_seed(seed.try_into().unwrap_or_else(|_| {
            // SHA-256 produces 32 bytes, exactly fits [u8; 32]
            unreachable!("hash provider always produces 32 bytes")
        }));
        let target: u64 = rng.gen_range(0..total);

        // Deterministic iteration: sort keys lexicographically
        let mut keys: Vec<&Vec<u8>> = stakers.stakers.keys().collect();
        keys.sort();

        let first_key = keys[0].clone(); // backup for fallback
        let mut cumulative = 0u64;
        for key in keys {
            let stake = *stakers.stakers.get(key).unwrap();
            cumulative += stake;
            if cumulative >= target {
                return key.clone();
            }
        }
        // Fallback: return the first staker (shouldn't happen if total > 0)
        first_key
    }
}

impl IEpochLeaderSelector for LeaderSelector {
    fn select(&self, stakers: &StakeSet, epoch_number: u64, prev_block_hash: &[u8]) -> Vec<u8> {
        self.select_internal(stakers, epoch_number, prev_block_hash)
    }
}

// ---------------------------------------------------------------------------
// EpochReconciler tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pneumatic_core::blocks::{Block, FinalityStatus};
    use pneumatic_core::data::{DataProvider, StubDataProvider};
    use pneumatic_core::epoch::{CandidateRegistry, IEpochReconciler};
    use pneumatic_core::epoch::StakingOp;
    use pneumatic_core::tokens::Token;
    use pneumatic_core::transactions::SignedTransaction;

    use super::{EpochReconciler, Logger, StakingManager, StakeStore};
    use pneumatic_core::epoch::IStakingManager;

    fn build_valid_block(prev_hash: Vec<u8>) -> Block {
        let signed = SignedTransaction::test_transaction();
        let mut block = Block {
            signed_trans: signed,
            token_metadata: std::collections::HashMap::new(),
            previous_hash: prev_hash,
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block);
        block
    }

    fn make_valid_token(id: Vec<u8>, chain_count: usize) -> Token {
        let mut token = Token::new();
        token.id = id;
        let mut prev_hash = vec![42u8; 32]; // genesis hash
        for _ in 0..chain_count {
            let block = build_valid_block(prev_hash);
            prev_hash = block.current_hash.clone();
            token.blockchain.add_block(block);
        }
        assert!(token.blockchain.get_current_chain_state().is_valid);
        token
    }

    fn make_invalid_token(id: Vec<u8>) -> Token {
        let mut token = make_valid_token(id, 2);
        if let Some(block) = token.blockchain.chain.back_mut() {
            block.current_hash = vec![99u8; 32];
        }
        assert!(!token.blockchain.get_current_chain_state().is_valid);
        token
    }

    /// An invalid chain whose tip block carries a specific `proposer_key`, so the
    /// test can assert exactly who `reconcile_internal` slashes.
    fn make_invalid_token_with_proposer(id: Vec<u8>, tip_proposer: Vec<u8>) -> Token {
        let mut token = make_valid_token(id, 2);
        if let Some(block) = token.blockchain.chain.back_mut() {
            block.proposer_key = tip_proposer.clone();
        }
        if let Some(block) = token.blockchain.chain.back_mut() {
            block.current_hash = vec![99u8; 32];
        }
        assert!(!token.blockchain.get_current_chain_state().is_valid);
        token
    }

    fn make_stake_store(stakes: Vec<(Vec<u8>, u64)>) -> Arc<StakeStore> {
        let store = Arc::new(StakeStore::new());
        for (key, stake) in stakes {
            store.add_staker(key, stake);
        }
        store
    }

    fn make_reconciler(
        stakes: Vec<(Vec<u8>, u64)>,
        tokens: Vec<(Vec<u8>, Token)>,
        token_ids: Vec<Vec<u8>>,
        slash_fraction: f64,
    ) -> (EpochReconciler, Arc<StakeStore>, Arc<CandidateRegistry>) {
        let stake_store = make_stake_store(stakes);
        let registry = Arc::new(CandidateRegistry::new());
        let mut stub = StubDataProvider::new();
        for (key, token) in tokens {
            stub = stub.with_token(key, "test".to_string(), token);
        }
        let data_provider: Arc<dyn DataProvider> = Arc::new(stub);
        let reconciler = EpochReconciler::new(
            stake_store.clone(),
            registry.clone(),
            data_provider,
            "test".to_string(),
            token_ids,
            slash_fraction,
        );
        (reconciler, stake_store, registry)
    }

    // --- Basic reconciliation ---

    #[test]
    fn reconcile_empty_token_ids_returns_default() {
        let (reconciler, _, _) = make_reconciler(vec![], vec![], vec![], 1.0);
        let result = reconciler.reconcile();
        assert!(result.misshapen_tokens.is_empty());
        assert!(result.finalization_conflicts.is_empty());
    }

    #[test]
    fn reconcile_token_not_found_skipped() {
        let (reconciler, _, _) = make_reconciler(vec![], vec![], vec![vec![1], vec![2]], 1.0);
        let result = reconciler.reconcile();
        assert!(result.misshapen_tokens.is_empty());
        assert!(result.finalization_conflicts.is_empty());
    }

    #[test]
    fn reconcile_valid_chain_not_misshapen() {
        let token = make_valid_token(vec![1], 3);
        let (reconciler, _, _) = make_reconciler(vec![], vec![(vec![1], token)], vec![vec![1]], 1.0);
        let result = reconciler.reconcile();
        assert!(!result.misshapen_tokens.contains(&vec![1]));
        assert!(result.misshapen_tokens.is_empty());
    }

    #[test]
    fn reconcile_invalid_chain_detected_as_misshapen() {
        let token = make_invalid_token(vec![1]);
        let (reconciler, _, _) = make_reconciler(vec![], vec![(vec![1], token)], vec![vec![1]], 1.0);
        let result = reconciler.reconcile();
        assert!(result.misshapen_tokens.contains(&vec![1]));
    }

    #[test]
    fn reconcile_misshapen_chain_slashes_tip_proposer() {
        // AUDIT Phase 5.2 / H2: `misshapen_tokens` is no longer a dead accumulator —
        // reconciling a token with an invalid chain must emit a real slash op against
        // the tip proposer AND still record the token in `misshapen_tokens`. This is
        // the discriminator: before the fix the branch only pushed the id and never
        // touched `slashing_ops`, so `slash` is `None`.
        let token = make_invalid_token_with_proposer(vec![1], vec![1]);
        let (reconciler, store, _) = make_reconciler(
            vec![(vec![1], 100)],
            vec![(vec![1], token)],
            vec![vec![1]],
            1.0, // full slash
        );
        let result = reconciler.reconcile();

        // The token is still recorded as misshapen (informational record preserved).
        assert!(result.misshapen_tokens.contains(&vec![1]));

        // A real slash op is emitted against the tip proposer — the tip's proposer
        // (vec![1]) has 100 stake, slash_fraction is 1.0 → full 100.
        let slash = result.slashing_ops.iter().find_map(|op| match op {
            StakingOp::Slash(key, amount) => Some((key.clone(), *amount)),
            _ => None,
        });
        assert_eq!(slash, Some((vec![1], 100)));

        // Applying the ops actually moves the StakeStore — slashing is real.
        let manager = StakingManager::new(store.clone(), Arc::new(NullLogger));
        manager.apply_ops(&result).expect("apply_ops should succeed");
        assert_eq!(store.get_stake(&vec![1]), 0);
    }

    // --- Same-chain conflict detection via CandidateRegistry ---

    #[test]
    fn reconcile_no_candidates_no_conflicts() {
        let (reconciler, _, _) = make_reconciler(
            vec![(vec![1], 100)],
            vec![(vec![1], make_valid_token(vec![1], 2))],
            vec![vec![1]],
            1.0,
        );
        // No candidates inserted → no conflicts
        let result = reconciler.reconcile();
        assert!(result.finalization_conflicts.is_empty());
    }

    #[test]
    fn reconcile_single_candidate_no_conflict() {
        let (reconciler, store, registry) = make_reconciler(
            vec![(vec![1], 100)],
            vec![(vec![1], make_valid_token(vec![1], 2))],
            vec![vec![1]],
            1.0,
        );
        let tip_hash = tip_hash_for(&reconciler);
        let block = build_valid_block(vec![1, 2, 3]);
        store.add_staker(vec![1], 100);
        registry.insert(vec![1], tip_hash, block, vec![1]);
        let result = reconciler.reconcile();
        assert!(result.finalization_conflicts.is_empty());
    }

    // --- Helper to get tip hash from first valid token ---

    fn tip_hash_for(reconciler: &EpochReconciler) -> Vec<u8> {
        for token_id in &reconciler.token_ids {
            if let Ok(token) = reconciler.data_provider.get_token(token_id, &reconciler.env_id) {
                let state = token.blockchain.get_current_chain_state();
                if state.is_valid && !state.last_hash_in.is_empty() {
                    return state.last_hash_in;
                }
            }
        }
        vec![0u8; 32]
    }

    #[test]
    fn reconcile_conflict_at_tip_detected() {
        let (reconciler, store, registry) = make_reconciler(
            vec![(vec![1], 100)],
            vec![(vec![1], make_valid_token(vec![1], 2))],
            vec![vec![1]],
            1.0,
        );
        let tip_hash = tip_hash_for(&reconciler);
        let block_a = build_valid_block(vec![1, 2, 3]);
        let block_b = build_valid_block(vec![4, 5, 6]);
        store.add_staker(vec![1], 100);
        store.add_staker(vec![2], 200);
        registry.insert(vec![1], tip_hash.clone(), block_a, vec![1]);
        registry.insert(vec![1], tip_hash, block_b, vec![2]);

        let result = reconciler.reconcile();
        assert_eq!(result.finalization_conflicts.len(), 1);
        let conflict = &result.finalization_conflicts[0];
        assert_ne!(conflict.block_a, conflict.block_b);
        assert_eq!(conflict.stake_a, 100);
        assert_eq!(conflict.stake_b, 200);
    }

    // --- Slash emission on same-proposer double-sign (AUDIT Phase 5.1 / H1) ---

    /// A no-op logger so a `StakingManager` can be constructed in tests without
    /// touching the filesystem.
    struct NullLogger;
    impl Logger for NullLogger {
        fn log(&self, _message: String) {}
    }

    #[test]
    fn reconcile_same_proposer_conflict_slashes_proposer() {
        // Reconciliation must emit AND apply a real slash op for a same-proposer
        // double-sign — previously reconcile_internal emitted zero slash ops.
        let (reconciler, store, registry) = make_reconciler(
            vec![(vec![1], 100)],
            vec![(vec![1], make_valid_token(vec![1], 2))],
            vec![vec![1]],
            1.0, // full slash
        );
        let tip_hash = tip_hash_for(&reconciler);
        // Two blocks by the SAME proposer at the same tip — a double-sign.
        let block_a = build_valid_block(vec![1, 2, 3]);
        let block_b = build_valid_block(vec![4, 5, 6]);
        store.add_staker(vec![1], 100);
        registry.insert(vec![1], tip_hash.clone(), block_a, vec![1]);
        registry.insert(vec![1], tip_hash, block_b, vec![1]);

        let result = reconciler.reconcile();
        // The conflict is recorded and a full slash op is emitted for the offender.
        assert_eq!(result.finalization_conflicts.len(), 1);
        let slash = result.slashing_ops.iter().find_map(|op| match op {
            StakingOp::Slash(key, amount) => Some((key.clone(), *amount)),
            _ => None,
        });
        assert_eq!(slash, Some((vec![1], 100)));

        // Applying the ops must actually move the StakeStore — slashing is real.
        let manager = StakingManager::new(store.clone(), Arc::new(NullLogger));
        manager.apply_ops(&result).expect("apply_ops should succeed");
        assert_eq!(store.get_stake(&vec![1]), 0);
    }

    #[test]
    fn reconcile_conflict_stake_resolution_returns_real_stakes() {
        let (reconciler, store, registry) = make_reconciler(
            vec![(vec![1], 500)],
            vec![(vec![1], make_valid_token(vec![1], 2))],
            vec![vec![1]],
            1.0,
        );
        let tip_hash = tip_hash_for(&reconciler);
        let block_a = build_valid_block(vec![10]);
        let block_b = build_valid_block(vec![20]);
        store.add_staker(vec![1], 100);
        store.add_staker(vec![2], 500);
        registry.insert(vec![1], tip_hash.clone(), block_a, vec![1]);
        registry.insert(vec![1], tip_hash, block_b, vec![2]);

        let result = reconciler.reconcile();
        assert_eq!(result.finalization_conflicts.len(), 1);
        let conflict = &result.finalization_conflicts[0];
        // Proposer 2 has more stake → should have higher stake in conflict
        assert_eq!(conflict.stake_a, 100);
        assert_eq!(conflict.stake_b, 500);
    }

    // --- Concurrent access ---

    #[test]
    fn reconcile_concurrent_token_access_no_panic() {
        let stakes: Vec<(Vec<u8>, u64)> = (0..5).map(|i| (vec![i], 100)).collect();
        let tokens: Vec<(Vec<u8>, Token)> = (0..5).map(|i| {
            (vec![i], make_valid_token(vec![i], 2))
        }).collect();
        let token_ids: Vec<Vec<u8>> = (0..5).map(|i| vec![i]).collect();
        let (reconciler, _, _) = make_reconciler(stakes, tokens, token_ids, 1.0);
        let result = std::thread::scope(|s| {
            let mut handles = vec![];
            for _ in 0..10 {
                let r = &reconciler;
                handles.push(s.spawn(|| r.reconcile()));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });
        // All joins succeeded → no data races
        for res in &result {
            assert!(res.misshapen_tokens.is_empty());
        }
    }
}
