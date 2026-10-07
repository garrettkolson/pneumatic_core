//! Finalization core for the finalizer: `try_finalize` and its
//! optimistic variant, plus the block-construction resolution helpers they
//! share (per-epoch stake set, previous hash, stake metrics).

use super::*;

impl Finalizer {
/// Get the stake set for the current epoch.
///
/// Priority: (1) manually set via `set_stake_set` (test override),
/// (2) cached/fetched from DataProvider (production path).
pub(crate) fn get_stake_set_for_epoch(&self) -> Option<StakeSet> {
    if let Some(s) = &self.stake_set {
        return Some(s.clone());
    }
    self.stake_cache.get(self.current_epoch)
}

/// Resolve the chain tip (`previous_hash`) for the token a transaction targets.
///
/// Reads `last_hash_in` from the token's current chain state. On an empty
/// chain (or an invalid one — `ChainState::invalid()` also yields `vec![]`)
/// this is `vec![]`, which is the correct genesis prev-hash for the
/// committer's strict linkage check. On lookup failure we fall back to
/// `vec![]` rather than failing finalization: a stale/empty prev-hash is
/// dropped non-fatally by committers anyway, while a hard error would
/// permanently stall the transaction (signatures are already collected).
///
/// Note: `get_token` clones the full token (chain length is bounded by the
/// token's `security_level`), which is acceptable at once-per-finalize.
pub(crate) fn resolve_previous_hash(&self, token_id: &Vec<u8>) -> Vec<u8> {
    match self.data_provider.get_token(token_id, &self.partition_id) {
        Ok(token) => token.blockchain.get_current_chain_state().last_hash_in,
        Err(e) => {
            log::warn!(
                "resolve_previous_hash: failed to load token for previous_hash lookup ({}): {}; falling back to empty prev-hash",
                bytes_to_hex(token_id),
                e
            );
            vec![]
        }
    }
}

/// Resolve `(total_stake, total_voters)` for the standard finalize path
/// from the current epoch's stake set (manual override or DataProvider
/// cache). Falls back to `(0, 0)` when no stake set is available.
pub(crate) fn resolve_stake_metrics(&self) -> (u64, u32) {
    match self.get_stake_set_for_epoch() {
        Some(set) => (set.total_stake(), set.stakers.len() as u32),
        None => (0, 0),
    }
}

/// The set whose votes may finalize a transaction in this epoch.
///
/// At `shard_count == 1` the sentinel assigns **every** positive-stake executor
/// (`src/epoch/leader.rs:181-194`), so the epoch stake set *is* the assigned set
/// and this is exact rather than an approximation.
///
/// `shard_count > 1` is **refused, not guessed**. A shard is chosen from the
/// per-transaction selection salt, which the finalizer is never told, so the only
/// set available here is the global one. Demanding the global set's stake from a
/// node that was asked to vote as part of a shard would fail every sharded
/// transaction — while looking exactly like a working quorum check, which is worse
/// than an honest error, because it would hide the real prerequisite: carrying a
/// selection record (epoch, salt, committee) to the nodes that must verify it.
/// That is Phase 7's "declare and verify the responsible set".
pub(crate) fn responsible_set(&self) -> Result<crate::signature_collector::ResponsibleSet, PneumaticError> {
    if self.env_data.shard_count > 1 {
        return Err(PneumaticError::Epoch(format!(
            "Cannot determine the responsible set for shard_count {}: per-shard finalization \
             requires the sentinel's selection record (epoch, salt, committee). Run with \
             shard_count 1, or implement the selection record first.",
            self.env_data.shard_count
        )));
    }
    let stake_set = self.get_stake_set_for_epoch().ok_or_else(|| {
        PneumaticError::Epoch(
            "No stake snapshot available for the current epoch. Without a declared responsible \
             set, a quorum would be measured against whoever happened to vote — which is the \
             failure this replaces."
                .to_string(),
        )
    })?;
    Ok(crate::signature_collector::ResponsibleSet::from_stake_set(&stake_set))
}

/// Attempt to finalize a transaction after quorum is reached.
///
/// **This function has no caller in the workspace** (verified by grep, 10/07/2026). The
/// live standard-token path is `try_finalize_optimistic` above, which per ADR-005/ADR-010
/// commits on the first authenticated executor vote and waits for no quorum — so this
/// quorum-gated path is not what governs an ordinary transaction, and a defect audit that
/// stops here will miss the committer-side gate that does
/// (`fact-committer-confirmation-gate-did-not-gate`).
///
/// It is kept rather than deleted because ADR-005 retains the quorum machinery as the
/// *conflict-resolution* path, and this is its only implementation. Wiring it to the
/// conflict case, or removing it along with the machinery beneath it, is a deliberate
/// Phase 7 decision — not a cleanup to perform as a side effect.
///
/// This is the core pipeline step:
/// 1. Reconcile executor signatures
/// 2. Build the SignedTransaction
/// 3. Sign the finalizer's portion
/// 4. Build the Block
/// 5. Send to Committers
/// 6. Clear Sentinels
async fn try_finalize(&self, tx_id: &str) -> Result<Vec<u8>, PneumaticError> {
    // Step 1: Reconcile collected signatures against the set that was responsible
    // for this transaction. Naming that set is what makes the quorum mean
    // something: measured against the votes that arrived, one vote was always 100%.
    let responsible = self.responsible_set()?;
    let reconciled = self.signature_collector.reconcile_signatures(tx_id, &responsible)?;

    // Step 2: Load the transaction from pending registry
    let entry = self.pending_registry.get_transaction_mut(tx_id)?;
    let transaction = match entry.state {
        TransactionState::Preloaded { ref transaction }
        | TransactionState::Validated { ref transaction, .. }
        | TransactionState::Executing { ref transaction }
        // P10 (composite shared registry): the executor transitions the
        // shared entry to Finalizing at the end of execution (step 10)
        // before its "Sign" vote arrives, so the finalizer must accept an
        // already-Finalizing entry. In split deployments the finalizer's
        // own copy is Preloaded/Validated/Executing — Finalizing is
        // unreachable there until this very call.
        | TransactionState::Finalizing { ref transaction, .. } => {
            transaction.clone()
        }
        _ => {
            return Err(PneumaticError::Registry(format!(
                "Transaction {} not in executable state for finalization",
                tx_id
            )));
        }
    };
    drop(entry);

    // Step 3: Get the finalizer key from the transaction state
    let finalizer_key = match self.pending_registry.get_transaction_mut(tx_id) {
        Ok(entry) => match &entry.state {
            TransactionState::Validated { validation, .. } => {
                validation.finalizer_public_key.clone()
            }
            TransactionState::Finalizing { finalizer_key, .. } => {
                finalizer_key.clone()
            }
            _ => vec![],
        },
        Err(_) => vec![],
    };

    // Step 4: Build SignedTransaction from reconciled data, using the
    // current epoch's stake set for the voter metrics.
    let (total_stake, total_voters) = self.resolve_stake_metrics();

    let mut signed_tx = self.block_builder.build_signed_transaction(
        &reconciled,
        &transaction,
        total_stake,
        total_voters,
    );

    // Step 5: Sign the finalizer's portion
    let finalizer_sig = self.block_builder.sign_finalizer_block(&mut signed_tx).await?;

    signed_tx.finalizer_sig = finalizer_sig;

    // Step 6: Transition to Finalizing state
    if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
        entry.transition_to_finalizing(transaction.clone(), finalizer_key);
    }

    // Step 7: Create the Block, chained to the token's current chain tip.
    let previous_hash = self.resolve_previous_hash(&transaction.token_id);
    let block = self.block_builder.create_block(signed_tx.clone(), previous_hash, self.current_epoch)?;

    // Step 8: Send the commit to all Committers
    let block_hash = block.current_hash.clone();
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: transaction.token_id.clone(),
        env_id: self.env_id.clone(),
        proposed_block: block,
    };
    self.message_dispatcher.send_to_committers(commit).await?;

    // Step 9: Send clear to all Sentinels
    self.message_dispatcher.send_clear_to_sentinels(tx_id).await?;

    // Step 10: Transition to Committed state
    if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
        entry.transition_to_committed(transaction, block_hash);
    }

    // Clean up preload tasks
    self.preload_tasks.lock().await.remove(tx_id);

    // Clean up signature registry
    let _ = self.signature_registry.try_remove_transaction(tx_id);

    // Acknowledge success
    Ok(serialize_to_bytes_rmp(&tx_id.as_bytes().to_vec())
        .map_err(|e| PneumaticError::Encoding(e.to_string()))?)
}

/// Attempt to finalize a transaction optimistically (first executor signature).
///
/// This is the fast path — no quorum waiting, no signature reconciliation.
/// The single executor's honest signature is proof enough for optimistic commit.
/// Subsequent signatures accumulate stake in the background.
///
/// `voter_pubkey` is the *authenticated* voter (verified in `handle_signature`):
/// the envelope signature verified and the key is a registered `Executor`.
pub(crate) async fn try_finalize_optimistic(
    &self,
    tx_id: &str,
    single_sig: &TransactionSignature,
    voter_pubkey: &[u8],
) -> Result<Vec<u8>, PneumaticError> {
    // Step 1: Load the transaction from pending registry
    let entry = self.pending_registry.get_transaction_mut(tx_id)?;
    let transaction = match entry.state {
        TransactionState::Preloaded { ref transaction }
        | TransactionState::Validated { ref transaction, .. }
        | TransactionState::Executing { ref transaction }
        // P10 (composite shared registry): the executor leaves the shared
        // entry in Finalizing (execution step 10) before this vote arrives —
        // the optimistic path must accept it. The entry's transaction is the
        // executor's stamped view (result_data set); `result_hash` is stamped
        // below and written back via the Finalizing/Committed transitions.
        | TransactionState::Finalizing { ref transaction, .. } => {
            transaction.clone()
        }
        _ => {
            return Err(PneumaticError::Registry(format!(
                "Transaction {} not in executable state for finalization",
                tx_id
            )));
        }
    };
    drop(entry);

    // P10: stamp the verified executor vote hash onto the canonical tx
    // BEFORE it goes into the block, so the shared-registry entry and the
    // wire block the finalizer commits carry the identical transaction. In
    // the composite (one shared pending registry) the committer's H12
    // hash-match compares the registry entry against the wire block's
    // embedded tx; an unstamped registry copy would mismatch and fail
    // closed. In a split deployment the committer materializes its own
    // entry from the wire block (H4), so the stamp is harmless there.
    let mut transaction = transaction;
    transaction.result_hash = single_sig.transaction_hash.clone();

    // Step 2: Get the finalizer key from the transaction state
    let finalizer_key = match self.pending_registry.get_transaction_mut(tx_id) {
        Ok(entry) => match &entry.state {
            TransactionState::Validated { validation, .. } => {
                validation.finalizer_public_key.clone()
            }
            TransactionState::Finalizing { finalizer_key, .. } => {
                finalizer_key.clone()
            }
            _ => vec![],
        },
        Err(_) => vec![],
    };

    // Step 3: Build SignedTransaction using the single authenticated voter's
    // signature.
    let mut signed_tx = self.block_builder.build_signed_transaction_optimistic(
        single_sig,
        &transaction,
        voter_pubkey,
    );

    // Step 4: Sign the finalizer's portion
    let finalizer_sig = self.block_builder.sign_finalizer_block(&mut signed_tx).await?;
    signed_tx.finalizer_sig = finalizer_sig;

    // Step 5: Transition to Finalizing state
    if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
        entry.transition_to_finalizing(transaction.clone(), finalizer_key);
    }

    // Step 6: Create the Block with optimistic finality, chained to the
    // token's current chain tip.
    let previous_hash = self.resolve_previous_hash(&transaction.token_id);
    let block = self.block_builder.create_block_optimistic(
        signed_tx.clone(),
        previous_hash,
        self.current_epoch,
    )?;

    // Step 7: Send the commit to all Committers
    let block_hash = block.current_hash.clone();
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: transaction.token_id.clone(),
        env_id: self.env_id.clone(),
        proposed_block: block.clone(),
    };
    self.message_dispatcher.send_to_committers(commit).await?;

    // Step 7.5: Broadcast block finalized to all committers and archivars via gossip.
    // Fetches the current epoch's stake set (cached) so receiving nodes
    // can perform stake-weighted confirmation tracking.
    let stake_set = self.get_stake_set_for_epoch();
    self.message_dispatcher
        .send_block_finalized(block, stake_set)
        .await?;

    // Step 8: Send clear to all Sentinels
    self.message_dispatcher.send_clear_to_sentinels(tx_id).await?;

    // Step 9: Transition to Committed state
    if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
        entry.transition_to_committed(transaction, block_hash);
    }

    // Clean up preload tasks
    self.preload_tasks.lock().await.remove(tx_id);

    // Clean up signature registry
    let _ = self.signature_registry.try_remove_transaction(tx_id);

    // Acknowledge success
    Ok(serialize_to_bytes_rmp(&tx_id.as_bytes().to_vec())
        .map_err(|e| PneumaticError::Encoding(e.to_string()))?)
}
}
