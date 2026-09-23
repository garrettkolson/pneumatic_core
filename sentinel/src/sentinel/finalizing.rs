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

        // Assign a new finalizer deterministically using the current stake snapshot.
        // Falls back to random candidate selection if the snapshot is unavailable.
        let new_key = match self.assign_finalizer_deterministic_retry(&tx_id, *self.current_epoch.lock(), &rejected_key) {
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

        // Transition to Finalizing with the new finalizer key.
        if let Ok(mut entry) = self.registry.get_transaction_mut(&tx_id) {
            let old_state = std::mem::replace(
                &mut entry.state,
                TransactionState::Pending,
            );
            if let TransactionState::Finalizing { transaction, .. } = old_state {
                entry.transition_to_finalizing(transaction, new_key.clone());
            } else {
                entry.state = old_state;
                let _ = self.registry.release_transaction(&tx_id);
                return Err(SentinelError::Registry(format!(
                    "Transaction {} not in Finalizing state for rejection handling", tx_id
                )));
            }
        }

        // Send the transaction to the new finalizer.
        if let Ok(tx) = self.registry.get_transaction(&tx_id) {
            let _ = self.transaction_notifier.request_single_finalizer(
                &tx, new_key.clone(), &self.env_data
            );
        }

        // Notify all sentinels that this transaction is being reassigned.
        let _ = self.transaction_notifier.notify_delete(&tx_id, &self.env_data);

        // Release lock — transaction remains in Finalizing for new finalizer.
        let _ = self.registry.release_transaction(&tx_id);

        Ok(())
    }

    /// Deterministically assign a finalizer for a transaction using the current
    /// stake snapshot. Returns the assigned finalizer's public key.
    ///
    /// Uses the sentinel's `EpochSnapshotCache<StakeSet>` to load the snapshot for the
    /// given epoch, then delegates to `pneumatic_core::deterministic_select`.
    ///
    /// If the snapshot is not cached and the DataProvider call fails, returns
    /// a `Routing` error.
    pub fn assign_finalizer_deterministic(
        &self,
        tx_id: &str,
        epoch_number: u64,
    ) -> Result<Vec<u8>, SentinelError> {
        let snapshot = self.stake_snapshot_cache.get(epoch_number)
            .ok_or_else(|| SentinelError::Routing(format!("No snapshot for epoch {}", epoch_number)))?;

        if snapshot.total_stake() == 0 {
            return Err(SentinelError::Routing("Stake set is empty".into()));
        }

        let prev_block_hash = self
            .data_provider
            .latest_block_hash(&self.env_data.environment_id)
            .unwrap_or_default() // unknown tip / I/O error → empty salt (genesis fails closed)
            .unwrap_or_default();

        let finalizer_key = pneumatic_core::deterministic_select(
            &snapshot,
            FINALIZER_DOMAIN,
            tx_id.as_bytes(),
            epoch_number,
            &prev_block_hash,
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
    ) -> Result<Vec<u8>, SentinelError> {
        let key = self.assign_finalizer_deterministic(tx_id, epoch_number)?;
        if key == rejected_key {
            // Try with a "retry" suffix to shift the selection
            let retry_tx_id = format!("{}_retry", tx_id);
            return self.assign_finalizer_deterministic(&retry_tx_id, epoch_number);
        }
        Ok(key)
    }

}
