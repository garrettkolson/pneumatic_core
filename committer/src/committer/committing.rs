//! Production commit-path logic for the Committer, extracted from
//! `crate::committer` to keep that module focused on routing and construction.
//!
//! These are `impl Committer` methods, so they retain access to the struct's
//! private fields (child modules of `crate::committer`).

use super::*;

impl Committer {

    /// Handle a "Commit" message — the core pipeline step.
    ///
    /// Flow:
    /// 1. Deserialize the TransactionCommit from the message body
    /// 2. Validate the transaction message (env_id, block hash)
    /// 3. Acquire lock and verify Finalizing state
    /// 4. Commit the block via BlockServices
    /// 5. Transition to Committed, release lock
    pub(crate) async fn handle_commit(&self, message: Message) -> Result<(), CommitterError> {
        // Deserialize the TransactionCommit from the message body
        let commit: TransactionCommit =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        // Validate the transaction message
        self.validate_transaction_message(&commit)?;

        // Check and commit the transaction results. The authenticated sender key (verified by
        // `authenticate_message`) is threaded through as the finalizer key: it identifies the
        // node that actually authenticated this Commit, which is more trustworthy than the
        // `finalizer_addr` self-declared inside the wire block.
        self.check_and_commit_transaction_results(&commit, message.public_key.clone()).await
    }

    /// Check and commit transaction results.
    ///
    /// Accepts transactions in two states:
    /// - **Finalizing**: standard pipeline (Sentinel → Executor → Finalizer → Committer).
    ///   The transaction was assigned a finalizer and collected quorum signatures.
    /// - **Validated**: leader-proposal path. The leader dequeued the transaction
    ///   from the pool and proposed it directly. No finalizer key is present.
    ///
    /// Flow:
    /// 1. Acquire lock on the transaction in the pending registry
    /// 2. Verify the transaction is in Finalizing OR Validated state
    /// 3. Apply the block via BlockServices (commit + distribute)
    /// 4. Update transaction state to Committed (or remove from pool for leader path)
    /// 5. Release the transaction lock
    pub(crate) async fn check_and_commit_transaction_results(
        &self,
        commit: &TransactionCommit,
        finalizer_key: Vec<u8>,
    ) -> Result<(), CommitterError> {
        let tx_id = String::from_utf8_lossy(&commit.trans_id).to_string();

        // AUDIT Phase 4.1 / H4: sink path. In the live pipeline the pending registry is normally
        // populated upstream (e.g. by the sentinel), so this entry usually already exists here. But
        // a Commit may arrive with no registry entry — e.g. a self-contained Finalizer→Committer
        // flow, or an empty registry at boot. Rather than fail closed on `TransactionNotInFinalizing`
        // for a transaction that is otherwise authentic (envelope-verified, registered sender,
        // valid finalizer signature on the block), materialize it here as `Finalizing` from the wire
        // block's transaction, keyed to the authenticated finalizer. The H12 hash check below then
        // binds this transaction to the committed payload.
        if !self.pending_registry.contains(&tx_id) {
            let entry = PendingTransaction::new(
                tx_id.clone(),
                TransactionState::Finalizing {
                    transaction: commit.proposed_block.signed_trans.transaction.clone(),
                    finalizer_key: finalizer_key.clone(),
                },
            );
            self.pending_registry
                .add_transaction(tx_id.clone(), entry)
                .map_err(|_| CommitterError::TransactionNotInFinalizing(tx_id.clone()))?;
        }

        // Step 1: Acquire lock on the transaction
        self.pending_registry
            .acquire_transaction(&tx_id)
            .map_err(|_| CommitterError::TransactionNotInFinalizing(tx_id.clone()))?;

        // Step 2: Extract transaction from either Finalizing (standard pipeline)
        // or Validated (leader-proposal) state.
        let (transaction, is_leader_proposal) = {
            let entry = self
                .pending_registry
                .get_transaction_mut(&tx_id)?;

            match &entry.state {
                TransactionState::Finalizing { transaction, .. } => {
                    (transaction.clone(), false)
                }
                TransactionState::Validated { transaction, .. } => {
                    (transaction.clone(), true)
                }
                _ => {
                    return Err(CommitterError::TransactionNotInFinalizing(tx_id));
                }
            }
        };

        // AUDIT Phase 3.5 / H12: commit the validated payload, not whatever arrived. The wire
        // TransactionCommit carries its own `proposed_block`; that block is what actually gets
        // appended to the chain, yet nothing above verifies its embedded transaction is the one we
        // validated and pooled. Hash-compare the wire block's transaction against the validated one
        // (a full-payload hash, so a swap of any field is caught) — fail closed on any mismatch.
        if transaction.hash()? != commit.proposed_block.signed_trans.transaction.hash()? {
            return Err(CommitterError::TransactionPayloadMismatch(tx_id.clone()));
        }

        // Step 3: Check for conflicts and resolve before committing. The resolved
        // conflict tells us whether the incoming block wins outright (`Commit`) or
        // wins only after the losing tip is rolled back (`CommitWinnerAfterRollback`)
        // — or was rejected (`LoserDiscarded`, surfaced below).
        match self.handle_conflict_at_commit(commit, &finalizer_key) {
            Ok(CommitConflictOutcome::Commit) => {
                self.block_services.commit_block(commit, None)?;
            }
            Ok(CommitConflictOutcome::CommitWinnerAfterRollback(loser_hash)) => {
                self.block_services.commit_block(commit, Some(loser_hash))?;
            }
            Err(e) => return Err(e),
        }

        // Step 3.5: Deduct gas from sender's fuel balance.
        //
        // AUDIT Phase 4.5 / M11: fail-closed. Any failure to read or persist the
        // sender's balance is surfaced (a loud log line + a returned
        // `CommitterError::GasDeduction`) rather than silently swallowed, so gas is
        // never given for free. The per-sender lock serializes the read-modify-write so
        // concurrent commits for the same sender cannot lose an update. The `if let
        // Some` guard is deliberate: a tx with no tracked gas (never routed through the
        // cost model) is not debited and is not an error. The partition is the
        // environment's token partition (the same one `verify_gas` debited at admission).
        if let Some(gas_used) = self.pending_registry.get_gas_used(&tx_id) {
            let mutex = self.rmw_mutex(&transaction.sender).await;
            let _guard = mutex.lock().unwrap_or_else(|p| p.into_inner());
            let user = self
                .data_provider
                .get_user(&transaction.sender, &self.env_data.token_partition_id)
                .map_err(|e| self.gas_deduction_err(&transaction.sender, &tx_id, gas_used, &e))?;
            let mut user = user;
            user.fuel_balance = user.fuel_balance.saturating_sub(gas_used);
            self.data_provider.save_user(
                &transaction.sender,
                user,
                &self.env_data.token_partition_id,
            )
            .map_err(|e| self.gas_deduction_err(&transaction.sender, &tx_id, gas_used, &e))?;
        }

        // Step 4: Update transaction state
        if is_leader_proposal {
            // Leader-proposal path: remove from pool, then transition to Committed
            // so that release() returns true (entry cleanup requires Committed/Failed state).
            self.pending_registry.remove_from_pool(&tx_id);
        }
        // Transition to Committed for BOTH paths — release() checks for Committed/Failed
        // to decide whether to remove the entry when lock_count reaches 0.
        if let Ok(mut entry) = self.pending_registry.get_transaction_mut(&tx_id) {
            // AUDIT Phase 3.5 / H12: `block_hash` holds the hash of the block the transaction was
            // committed *into* — here the committed block's own hash (the finalizer already stores
            // this). Previously `result.token_id` (a token id) was stored, a misnomer.
            entry.transition_to_committed(transaction, commit.proposed_block.current_hash.clone());
        }

        // Step 5: Release the transaction lock
        let should_remove = self.pending_registry.release_transaction(&tx_id)?;

        if should_remove {
            let _ = self.pending_registry.remove_transaction(&tx_id);
        }

        // Step 6: Distribute the committed block to archivers
        let _ = self.block_services.distribute_to_archivers(&commit.proposed_block).await;

        Ok(())
    }


    /// Check for and resolve block conflicts at commit time.
    ///
    /// Before committing a block, check the CandidateRegistry for competing
    /// proposals at the same (token_id, previous_hash). If a conflict is
    /// detected, resolve it using stake-weighted selection with proposer
    /// identity branching:
    /// - Different proposers → DiscardLoser (network race)
    /// - Same proposer → SameProposerSlash (double-signed)
    /// - Equal stakes + different proposers → TieFlagBoth (fail-closed; the old
    ///   equal-stake hash tie-break was removed as attacker-grindable — Phase 5.8)
    ///
    /// Every resolved group is then cleared from the registry via
    /// `remove_conflicted`, and the loser is never left standing (AUDIT Phase
    /// 5.2 / H2): the incoming block commits only if it wins — appended as-is
    /// (outcome `Commit`) or, if the loser happens to be the current tip, after
    /// that tip is rolled back (outcome `CommitWinnerAfterRollback`). Any other
    /// resolution rejects the incoming block with `Err(CommitterError::LoserDiscarded)`.
    pub(crate) fn handle_conflict_at_commit(
        &self,
        commit: &TransactionCommit,
        verified_proposer: &[u8],
    ) -> Result<CommitConflictOutcome, CommitterError> {
        let token_id = commit.token_id.clone();
        let previous_hash = commit.proposed_block.previous_hash.clone();
        let incoming_hash = commit.proposed_block.current_hash.clone();

        // Check for existing candidates at this position
        let candidates = self.candidate_registry
            .get_candidates(&token_id, &previous_hash);

        if candidates.is_empty() {
            // No conflict — this is the first candidate for this position.
            // Insert it into the registry for future conflict detection. The
            // incoming block is the winner by default (no rollback needed).
            //
            // Record the *verified* proposer (the authenticated envelope sender,
            // `message.public_key`) — never the self-declared `proposer_key`, which
            // is unsigned and could be forged to inflate stake or mis-trigger a
            // slash in a later conflict resolution. (AUDIT Phase 5.8 / M10)
            self.candidate_registry.insert(
                token_id, previous_hash,
                commit.proposed_block.clone(),
                verified_proposer.to_vec(),
            );
            return Ok(CommitConflictOutcome::Commit);
        }

        // Conflict detected — fold the incoming block over ALL candidates (AUDIT
        // Phase 5.8 / M10): the incoming wins only if it beats every candidate, and
        // losing to any one rejects it. Build the StakeSet once for resolution.
        let stake_set = StakeSet {
            stakers: self.stake_store.iter()
                .map(|(k, s)| (k.clone(), s))
                .collect(),
        };

        // Reject the incoming block if it loses to ANY candidate, or if any pair is a
        // SameProposerSlash or TieFlagBoth (fail-closed). Resolution uses the verified
        // proposer, never the self-declared field.
        let mut rollback_target = incoming_hash.clone();
        for (candidate, candidate_proposer) in &candidates {
            let candidate_hash = candidate.current_hash.clone();

            match resolve_block_conflict(
                &incoming_hash, &candidate_hash,
                verified_proposer, candidate_proposer,
                &stake_set,
            ).map_err(|e| CommitterError::Core(e))? {
                // Incoming loses to this candidate (winner is the candidate) — reject it.
                ConflictResolution::DiscardLoser(winner_hash) if winner_hash != incoming_hash => {
                    self.env_data.logger.log(format!(
                        "Conflict resolved (DiscardLoser) at commit: candidate {} beats incoming {} (token: {})",
                        bytes_to_hex(&candidate_hash), bytes_to_hex(&incoming_hash), bytes_to_hex(&token_id),
                    ));
                    self.candidate_registry.remove_conflicted(&token_id, &previous_hash);
                    return Err(CommitterError::LoserDiscarded);
                }
                // Incoming wins vs this candidate — record the buffered loser as the rollback
                // target and continue folding over the rest.
                ConflictResolution::DiscardLoser(_) => rollback_target = candidate_hash,
                // Same verified proposer double-signed — slash and reject incoming.
                ConflictResolution::SameProposerSlash(_, slashed_key) => {
                    let amount = (self.stake_store.get_stake(&slashed_key) as f64
                        * self.env_data.cost_model.slash_fraction)
                        .round()
                        .min(u64::MAX as f64) as u64;

                    self.env_data.logger.log(format!(
                        "Double-proposal detected (same proposer) at commit: slashing {} (candidate {} vs incoming {})(token: {})",
                        bytes_to_hex(&slashed_key), bytes_to_hex(&candidate_hash), bytes_to_hex(&incoming_hash), bytes_to_hex(&token_id),
                    ));
                    self.staking_manager.apply_ops(&pneumatic_core::epoch::EpochReconciliation {
                        misshapen_tokens: vec![],
                        finalization_conflicts: vec![],
                        slashing_ops: vec![pneumatic_core::epoch::StakingOp::Slash(
                            slashed_key, amount,
                        )],
                        reward_ops: vec![],
                    })?;

                    self.candidate_registry.remove_conflicted(&token_id, &previous_hash);
                    return Err(CommitterError::LoserDiscarded);
                }
                // Equal stakes + different verified proposers — fail closed: flag both
                // for review and reject the incoming block.
                ConflictResolution::TieFlagBoth(_) => {
                    self.env_data.logger.log(format!(
                        "Tie conflict at commit — equal stakes + different verified proposers, flagging both for review and rejecting incoming (token: {})",
                        bytes_to_hex(&token_id),
                    ));
                    self.candidate_registry.remove_conflicted(&token_id, &previous_hash);
                    return Err(CommitterError::LoserDiscarded);
                }
            }
        }

        // Incoming beats every candidate — commit it. rollback_target is the buffered loser's
        // hash (the tip that the incoming displaces). commit_block rolls back only when that
        // target matches the current tip — a buffered candidate that is not on the chain leaves
        // the tip untouched, so the incoming appends to the real tip. (AUDIT Phase 5.2 / H2,
        // 5.8 / M10: the rollback target is the loser, never the winner's own hash.)
        self.candidate_registry.remove_conflicted(&token_id, &previous_hash);
        Ok(CommitConflictOutcome::CommitWinnerAfterRollback(rollback_target))
    }


    /// Validate a Commit message.
    ///
    /// Checks:
    /// 1. The commit's env_id matches this committer's environment
    /// 2. The proposed block's hash is non-empty
    pub(crate) fn validate_transaction_message(&self, commit: &TransactionCommit) -> Result<(), CommitterError> {
        // Environment ID check
        if commit.env_id != self.env_data.environment_id {
            return Err(CommitterError::EnvironmentMismatch {
                expected: self.env_data.environment_id.clone(),
                got: commit.env_id.clone(),
            });
        }

        // Block hash check
        if commit.proposed_block.current_hash.is_empty() {
            return Err(CommitterError::InvalidBlockHash);
        }

        Ok(())
    }
}
