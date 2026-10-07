//! Finalizer lifecycle for the Sentinel (confirmation, rejection, deterministic
//! finalizer assignment and retry).
//!
//! These are `impl Sentinel` methods, so they retain access to the struct's
//! private fields (child modules of `crate::sentinel`).

use super::*;

impl Sentinel {
    /// Handle a "Confirm" message — a finalizer has confirmed transaction processing.
    pub(crate) fn handle_confirmation(&self, message: Message) -> Result<(), SentinelError> {
        let tx_id: String = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // Acquire the transaction to prevent concurrent access during transition.
        if self.registry.acquire_transaction(&tx_id).is_err() {
            return Err(SentinelError::TransactionInTerminalState(tx_id.clone()));
        }

        // Verify the sender (message.public_key) matches the assigned finalizer.
        let sender_key = message.public_key.clone();
        if !self.registry.is_requested_finalizer(&tx_id, &sender_key) {
            return Err(SentinelError::Registry(format!(
                "Confirmation from unassigned finalizer {:?} for tx {}",
                sender_key, tx_id
            )));
        }

        // Transition to Committed state.
        if let Ok(mut entry) = self.registry.get_transaction_mut(&tx_id) {
            // Extract the transaction from Finalizing state to move it to Committed.
            let old_state = std::mem::replace(
                &mut entry.state,
                TransactionState::Pending,
            );
            if let TransactionState::Finalizing { transaction, .. } = old_state {
                entry.transition_to_committed(transaction, vec![]); // block_hash not yet available
            } else {
                entry.state = old_state;
                return Err(SentinelError::Registry(format!(
                    "Transaction {} not in Finalizing state for confirmation", tx_id
                )));
            }
        }

        // Notify all other sentinels that this transaction is committed.
        let _ = self.transaction_notifier.notify_delete(&tx_id, &self.env_data);

        // Release lock — transaction will be cleaned up after commit.
        let _ = self.registry.release_transaction(&tx_id);

        Ok(())
    }

    /// Handle a "Reject" message — a finalizer rejected the transaction.
    /// Reassign to a different finalizer using risk-based selection.
    pub(crate) fn handle_rejection(&self, message: Message) -> Result<(), SentinelError> {
        let tx_id: String = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // Acquire the transaction to prevent concurrent access during reassignment.
        if self.registry.acquire_transaction(&tx_id).is_err() {
            return Err(SentinelError::TransactionInTerminalState(tx_id.clone()));
        }

        // Verify the rejecting finalizer was actually the assigned one.
        let rejected_key = message.public_key.clone();
        if !self.registry.is_requested_finalizer(&tx_id, &rejected_key) {
            return Err(SentinelError::Registry(format!(
                "Rejection from non-assigned finalizer {:?} for tx {}",
                rejected_key, tx_id
            )));
        }

        // Assign a new finalizer deterministically against the current stake
        // snapshot, salted by the tip of the chain this transaction extends.
        //
        // The salt read is deliberately INSIDE the fallible attempt: if it cannot be
        // resolved, reassignment takes the same candidate-based fallback that a
        // missing stake snapshot already takes. What it must never do is substitute an
        // empty salt — that is indistinguishable from genesis and would freeze the
        // selection seed forever (ADR-019). `selection_tip` is what enforces that.
        let new_key = match self.reassign_finalizer_deterministically(&tx_id, &rejected_key) {
            Ok(key) => key,
            Err(_) => {
                // Fallback: pick the first non-rejected candidate from the node registry.
                let Some(nodes) = self.node_registry.get_nodes(&NodeRegistryType::Finalizer) else {
                    let _ = self.registry.release_transaction(&tx_id);
                    return Err(SentinelError::NoTarget(NodeRegistryType::Finalizer));
                };

                let fallback_keys: Vec<Vec<u8>> = nodes.iter()
                    .filter_map(|entry| {
                        let entry_key = entry.key();
                        if entry_key != &rejected_key {
                            Some(entry_key.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                match fallback_keys.into_iter().next() {
                    Some(k) => k,
                    None => {
                        let _ = self.registry.release_transaction(&tx_id);
                        return Err(SentinelError::Registry(format!(
                            "No alternative finalizer available for tx {} after rejection", tx_id
                        )));
                    }
                }
            }
        };

        // Transition to Finalizing with the new finalizer key, keeping a copy of
        // the transaction to hand to the new finalizer.
        //
        // The copy must come from THIS scope. The follow-up `get_transaction` this
        // replaces could never succeed: that accessor only serves entries in the
        // Validated state (`src/registry/pending.rs:246-257`), and this handler has
        // just moved the entry into Finalizing. So the lookup failed on EVERY
        // rejection — a state precondition, not a race. Wrapped in `if let Ok(tx)`
        // the error was invisible and the reassignment send never ran: the
        // transaction was reassigned in local state and delivered to nobody, with
        // nothing logged and `Ok(())` returned. Nothing recovers it either — the
        // new finalizer never learns the transaction exists, so no confirmation and
        // no rejection ever comes back. The recovery path stranded the transaction
        // it was meant to rescue.
        let reassigned_tx = match self.registry.get_transaction_mut(&tx_id) {
            Ok(mut entry) => {
                let old_state = std::mem::replace(&mut entry.state, TransactionState::Pending);
                if let TransactionState::Finalizing { transaction, .. } = old_state {
                    let for_send = transaction.clone();
                    entry.transition_to_finalizing(transaction, new_key.clone());
                    Some(for_send)
                } else {
                    entry.state = old_state;
                    let _ = self.registry.release_transaction(&tx_id);
                    return Err(SentinelError::Registry(format!(
                        "Transaction {} not in Finalizing state for rejection handling", tx_id
                    )));
                }
            }
            Err(e) => {
                // Previously this fell through to the (always-skipped) send and
                // the handler reported Ok(()). Nothing can be reassigned without
                // reading the entry, so say so.
                let _ = self.registry.release_transaction(&tx_id);
                return Err(SentinelError::Registry(format!(
                    "Cannot read transaction {} to reassign its finalizer: {}", tx_id, e
                )));
            }
        };

        // Send the transaction to the new finalizer. The result was discarded
        // here before, which is how a dropped send became a silently stuck
        // transaction: the new finalizer never receives it, so no rejection ever
        // comes back and nothing else retries it.
        let send_result = match reassigned_tx {
            Some(tx) => self
                .transaction_notifier
                .request_single_finalizer(&tx, new_key.clone(), &self.env_data)
                .map_err(Into::into),
            None => Err(SentinelError::Registry(format!(
                "Cannot send reassignment for tx {}: no transaction to send", tx_id
            ))),
        };

        // Notify all sentinels that this transaction is being reassigned.
        let _ = self.transaction_notifier.notify_delete(&tx_id, &self.env_data);

        // Release lock — transaction remains in Finalizing for new finalizer.
        // Released BEFORE surfacing the send failure so the entry is not left
        // write-locked behind the error path.
        let _ = self.registry.release_transaction(&tx_id);

        if let Err(e) = send_result {
            self.env_data.logger.log(format!(
                "finalizer reassignment for tx {} to {:02x?} did not reach its target: {:?}",
                tx_id, new_key, e
            ));
            return Err(e);
        }

        Ok(())
    }

    /// Resolve the salt for a transaction already in the registry and reassign its
    /// finalizer deterministically.
    ///
    /// The registry entry is read inside a scope so the DashMap guard is dropped
    /// before the provider call — a shard lock must never be held across socket I/O.
    pub(crate) fn reassign_finalizer_deterministically(
        &self,
        tx_id: &str,
        rejected_key: &[u8],
    ) -> Result<Vec<u8>, SentinelError> {
        let token_id = {
            let entry = self
                .registry
                .get_transaction_mut(tx_id)
                .map_err(|e| {
                    SentinelError::Registry(format!(
                        "Cannot read transaction {} for finalizer reassignment: {}",
                        tx_id, e
                    ))
                })?;
            match &entry.state {
                TransactionState::Preloaded { transaction }
                | TransactionState::Validated { transaction, .. }
                | TransactionState::Executing { transaction, .. }
                | TransactionState::Finalizing { transaction, .. } => transaction.token_id.clone(),
                _ => {
                    return Err(SentinelError::Registry(format!(
                        "Cannot reassign finalizer for tx {}: no transaction in state",
                        tx_id
                    )));
                }
            }
        };
        let chain_tip = self.selection_tip(&token_id)?;
        self.assign_finalizer_deterministic_retry(
            tx_id,
            *self.current_epoch.lock(),
            rejected_key,
            &chain_tip,
        )
    }

    /// Deterministically assign a finalizer for a transaction using the current
    /// stake snapshot. Returns the assigned finalizer's public key.
    ///
    /// Uses the sentinel's `EpochSnapshotCache<StakeSet>` to load the snapshot for the
    /// given epoch, then delegates to `pneumatic_core::deterministic_select`.
    ///
    /// If the snapshot is not cached, returns a `Routing` error.
    ///
    /// `chain_tip` is the selection salt: the tip of the chain the transaction
    /// extends, resolved by the caller (`Sentinel::selection_tip` for a token id,
    /// `Sentinel::chain_tip_of` when the token is already loaded). Passing it in —
    /// rather than having this function guess which chain was meant — is what keeps
    /// the salt per-token; see `fact-self-referential-quorum-denominator` for why a
    /// predictable selection is not a benign property.
    pub fn assign_finalizer_deterministic(
        &self,
        tx_id: &str,
        epoch_number: u64,
        chain_tip: &[u8],
    ) -> Result<Vec<u8>, SentinelError> {
        let snapshot = self.stake_snapshot_cache.get(epoch_number)
            .ok_or_else(|| SentinelError::Routing(format!("No snapshot for epoch {}", epoch_number)))?;

        if snapshot.total_stake() == 0 {
            return Err(SentinelError::Routing("Stake set is empty".into()));
        }

        let finalizer_key = pneumatic_core::deterministic_select(
            &snapshot,
            FINALIZER_DOMAIN,
            tx_id.as_bytes(),
            epoch_number,
            chain_tip,
        )
        .ok_or_else(|| SentinelError::Routing("Selection returned none for non-empty stake set".into()))?;

        if snapshot.get_stake(&finalizer_key) == 0 {
            return Err(SentinelError::Routing("Assigned finalizer has zero stake".into()));
        }

        Ok(finalizer_key)
    }

    /// Assign a finalizer deterministically, with a retry suffix if the
    /// initial assignment matches a rejected finalizer.
    pub fn assign_finalizer_deterministic_retry(
        &self,
        tx_id: &str,
        epoch_number: u64,
        rejected_key: &[u8],
        chain_tip: &[u8],
    ) -> Result<Vec<u8>, SentinelError> {
        let key = self.assign_finalizer_deterministic(tx_id, epoch_number, chain_tip)?;
        if key == rejected_key {
            // Try with a "retry" suffix to shift the selection. The salt is carried
            // unchanged: only the tie-break input moves, so a retry stays bound to
            // the same chain tip the original assignment used.
            let retry_tx_id = format!("{}_retry", tx_id);
            return self.assign_finalizer_deterministic(&retry_tx_id, epoch_number, chain_tip);
        }
        Ok(key)
    }

}
