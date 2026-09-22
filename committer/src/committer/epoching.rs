//! Production commit-path logic for the Committer, extracted from
//! `crate::committer` to keep that module focused on routing and construction.
//!
//! These are `impl Committer` methods, so they retain access to the struct's
//! private fields (child modules of `crate::committer`).

use super::*;

impl Committer {

    /// Handle epoch reconciliation request.
    ///
    /// Flow:
    /// 1. Run the epoch reconciler to analyze chain state
    /// 2. Apply staking operations from reconciliation
    /// 3. Select new leader for the epoch
    pub(crate) async fn handle_epoch_reconcile(&self) -> Result<(), CommitterError> {
        // Run reconciliation
        let reconciliation = self.epoch_reconciler.reconcile();

        if !reconciliation.slashing_ops.is_empty() || !reconciliation.reward_ops.is_empty() {
            // Apply staking operations
            self.staking_manager.apply_ops(&reconciliation)?;
        }

        // Advance to the next epoch via the single guarded writer (AUDIT Phase 5.4
        // / H9/M8). `advance_epoch_to` binds the leader seed to the mined tip,
        // advances the detector (the authoritative epoch source), mirrors the new
        // number into the atomic counter, and persists the stake + executor
        // snapshots — so the wire path can never diverge from or rewind the
        // counter, and a snapshot persistence failure aborts the advance.
        let advanced = self.advance_epoch_to()?;

        // Log the newly selected leader if the advance produced one.
        if let Some(leader_key) = advanced {
            let logger = &self.env_data.logger;
            if !leader_key.is_empty() {
                logger.log(format!(
                    "Epoch leader selected: {}",
                    bytes_to_hex(&leader_key)
                ));
            }
        }

        Ok(())
    }


    /// Advance to a new epoch: bump the epoch number, select a new leader,
    /// and save the previous leader for stale block detection.
    ///
    /// Thin wrapper over `advance_epoch_to`, the single guarded epoch advance
    /// shared by this internal path and the wire `EpochReconcile` path (AUDIT
    /// Phase 5.4 / H9/M8). Returns the new leader on a real advance, `None` when
    /// the advance was rejected (same/older epoch number, or the detector lock was
    /// already held), and `Err(CommitterError::SnapshotPersist)` if a snapshot
    /// fails to persist.
    pub(crate) fn advance_epoch(&self) -> Result<Option<Vec<u8>>, CommitterError> {
        self.advance_epoch_to()
    }

    /// The one writer for the epoch number (AUDIT Phase 5.4 / H9/M8).
    ///
    /// The `EpochBoundaryDetector`'s epoch is the authoritative source of truth.
    /// Both the internal production path (`advance_epoch`) and the wire
    /// `EpochReconcile` path (`handle_epoch_reconcile`) funnel here, so they can
    /// never disagree on or rewind the epoch number, and the counter can never
    /// fall behind the detector.
    ///
    /// Returns:
    ///  - `Ok(Some(new_leader))` on a real advance (detector advanced, number
    ///    mirrored, snapshots persisted);
    ///  - `Ok(None)` when the advance is rejected — the target epoch is not
    ///    strictly greater than the authoritative value (a reused/rewinding
    ///    number), or the detector lock is already held by another writer;
    ///  - `Err(CommitterError::SnapshotPersist{...})` if a snapshot fails to
    ///    persist, so the advance aborts rather than proceeding on a possibly
    ///    stale snapshot.
    pub(crate) fn advance_epoch_to(&self) -> Result<Option<Vec<u8>>, CommitterError> {
        let stake_set = self.stake_store.to_stake_set();
        // Phase 5.3 / H3: bind the new leader seed to the mined chain tip so the
        // epoch's leader is only knowable once this tip is produced. Read the
        // current tip from the local token cache — the committer holds its chain
        // state there and does not persist it to the data service.
        let prev_block_hash = self
            .tokens
            .iter()
            .map(|entry| entry.value().blockchain.get_current_chain_state().last_hash_in)
            .next()
            .unwrap_or_default();

        // The detector lock serializes the two writers: the internal path and the
        // wire path cannot both be mid-advance, and it is released before
        // propose_blocks re-locks it for is_epoch_expired. try_lock fails closed —
        // if it is already held, treat the advance as a no-op rather than block.
        let mut detector = self
            .epoch_detector
            .try_lock()
            .map_err(|_| CommitterError::InternalSerialization)?;
        let detector = detector
            .as_mut()
            .ok_or(CommitterError::InternalSerialization)?;

        // The detector's epoch is authoritative; the mirrored counter must track
        // it. Reject an advance whose target does not strictly exceed both.
        let current_epoch_number = detector.current_epoch.epoch_number;
        let new_epoch_number = current_epoch_number + 1;
        let stored = self.current_epoch_number.load(Ordering::SeqCst);
        if stored >= new_epoch_number {
            // Either the counter has already advanced past this target (a reused
            // number) or it is ahead of the detector (the divergence this funnel
            // removes) — neither is a valid advance, so rewind is refused.
            self.env_data
                .logger
                .log(format!(
                    "REJECT EPOCH ADVANCE: epoch {} would not exceed stored {} (detector epoch {})",
                    new_epoch_number, stored, current_epoch_number
                ));
            return Ok(None);
        }

        detector.advance_to_new_epoch(
            self.leader_selector.as_ref(),
            &stake_set,
            self.epoch_duration,
            &prev_block_hash,
        );
        let new_leader = detector.current_epoch.leader_public_key.clone();
        // Mirror the authoritative epoch into the atomic counter — the two now
        // move together, so the counter can never lag the detector again.
        self.current_epoch_number.store(new_epoch_number, Ordering::SeqCst);

        // Persist both snapshots, surfacing (not swallowing) any persistence
        // failure so a stale or missing snapshot never silently enters the pipeline.
        self.data_provider
            .save_stake_snapshot(
                new_epoch_number,
                stake_set.clone(),
                &self.env_data.token_partition_id,
            )
            .map_err(|e| self.snapshot_save_err(new_epoch_number, "stake", &e))?;
        self.data_provider
            .save_executor_set(
                new_epoch_number,
                stake_set.to_executor_set(),
                &self.env_data.token_partition_id,
            )
            .map_err(|e| self.snapshot_save_err(new_epoch_number, "executor", &e))?;

        Ok(Some(new_leader))
    }

    /// Propose a batch of transactions for a given token.
    ///
    /// Steps:
    /// 1. Check epoch expiry — if expired, advance to new epoch
    /// 2. Verify this node is the current epoch leader
    /// 3. Dequeue transactions from the pool via BlockProposer
    /// 4. Build TransactionCommit for each dequeued transaction
    /// 5. Return commits for dispatch to the Finalizer
    pub async fn propose_blocks(
        &self,
        token_id: &[u8],
        limit: usize,
    ) -> Result<Vec<TransactionCommit>, CommitterError> {
        // Step 1: Check epoch expiry and advance if needed
        {
            let now = chrono::Utc::now().timestamp();
            let should_advance = {
                let detector = self.epoch_detector.lock().await;
                if let Some(ref d) = *detector {
                    d.is_epoch_expired(now)
                } else {
                    false
                }
            };

            if should_advance {
                match self.advance_epoch() {
                    Ok(Some(new_leader)) => {
                        let epoch_num = self.current_epoch_number.load(Ordering::SeqCst);
                        let logger = &self.env_data.logger;
                        logger.log(format!(
                            "Epoch advanced to {} (new leader: {}",
                            epoch_num,
                            bytes_to_hex(&new_leader)
                        ));
                    }
                    // Rejected (same/older epoch, or detector lock already held) or
                    // an aborting persistence error — propagate it so propose_blocks
                    // surfaces rather than silently re-runs a stale epoch.
                    Ok(None) => {}
                    Err(e) => return Err(e),
                }
            }
        }

        // Step 2: Check if this node is the current epoch leader
        let is_leader = {
            let detector = self.epoch_detector.lock().await;
            if let Some(ref d) = *detector {
                d.current_leader() == Some(self.public_key.as_slice())
            } else {
                false
            }
        };

        if !is_leader {
            return Ok(Vec::new());
        }

        // Step 3: Dequeue transactions from the pool
        let batch = self
            .block_proposer
            .propose_batch(&self.pending_registry, token_id, limit)
            .map_err(|e| CommitterError::Core(e))?;

        if batch.is_empty() {
            return Ok(Vec::new());
        }

        // Step 4: Build TransactionCommit for each dequeued transaction
        let env_id = self.env_data.environment_id.clone();
        let token_id_vec = token_id.to_vec();
        let mut commits = Vec::with_capacity(batch.len());

        // Resolve the real token and its current chain tip before building blocks
        let token_ref = self.tokens.get(token_id).ok_or_else(|| {
            CommitterError::TokenNotFound(bytes_to_hex(token_id))
        })?;
        let token = token_ref.value();
        let blockchain = token.blockchain.clone();
        let epoch_number = self.current_epoch_number.load(Ordering::SeqCst);

        for (tx, signed) in batch {
            let commit = TransactionCommit {
                trans_id: tx.id.into_bytes(),
                token_id: token_id_vec.clone(),
                env_id: env_id.clone(),
                proposed_block: Block::from_transaction(
                    signed,
                    blockchain.clone(),
                    token,
                    epoch_number,
                ),
            };
            commits.push(commit);
        }

        Ok(commits)
    }

    /// Run the epoch loop: iterate registered token IDs and propose blocks for each.
    pub async fn run_epoch_loop(&self) -> Result<(), CommitterError> {
        // Collect token IDs first: `commit_block` takes a write lock on a token entry
        // (via `self.tokens.get_mut`). Holding the `iter()` read guard across that write
        // would deadlock the shard. Gather the keys up front so no shard lock is held
        // while committing.
        let token_ids: Vec<Vec<u8>> = self.tokens.iter().map(|r| r.key().clone()).collect();
        for token_id in token_ids {
            // AUDIT Phase 4.1 / H4: consume the leader-proposed commits instead of discarding them.
            // Each tx was dequeued from the pool as `Validated` by `propose_blocks`, so it is already
            // in the registry — the same commit routine as the inbound Commit path (4.1a) commits it.
            let commits = self.propose_blocks(&token_id, 10).await?;
            for commit in commits {
                if let Err(e) = self
                    .check_and_commit_transaction_results(&commit, self.sender_public_key().clone())
                    .await
                {
                    self.logger().log(format!("Leader-propose commit error: {:?}", e));
                }
            }
        }
        Ok(())
    }
}
