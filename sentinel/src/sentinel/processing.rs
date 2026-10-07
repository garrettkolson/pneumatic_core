//! Transaction intake and routing for the Sentinel (Process, self-signed, clear,
//! executor preload, shard-aware routing, failure transitions).
//!
//! These are `impl Sentinel` methods, so they retain access to the struct's
//! private fields (child modules of `crate::sentinel`).

use super::*;

impl Sentinel {
    /// The chain tip of the token a transaction extends — the deterministic
    /// selection salt (Phase 5.3 / AUDIT H3).
    ///
    /// Deliberately **per-token**. A block-lattice has no environment-level tip: a
    /// transaction extends exactly one token's chain, so the transaction's own token
    /// is the only canonical choice, and honest nodes cannot disagree about which
    /// chain the salt names. The previous implementation asked the data provider for
    /// `latest_block_hash(environment_id)`, whose lookup key conflated a partition
    /// with a token id; genesis even seeded a placeholder token under the environment
    /// id (`GenesisSpec::seed_partition_token`) to make that lookup succeed. The
    /// placeholder's chain never advances, so the salt was pinned to the genesis tip
    /// forever — the opposite of "only knowable once the previous block is mined".
    /// That ambiguous accessor is gone, so the conflation cannot be reintroduced.
    ///
    /// An empty salt is legitimate *only* for a genuinely empty chain (genesis). A
    /// token that cannot be read is an error: a missing token must not silently
    /// become a constant seed, because a constant seed makes every committee in
    /// every epoch predictable forever.
    pub(crate) fn selection_tip(&self, token_id: &[u8]) -> Result<Vec<u8>, SentinelError> {
        let token = self
            .data_provider
            .get_token(&token_id.to_vec(), &self.env_data.token_partition_id)
            .map_err(|e| SentinelError::Registry(format!(
                "Cannot read token for selection salt: {}", e
            )))?;
        Ok(Self::chain_tip_of(&token))
    }

    /// The salt for a token already in hand — no provider round trip. Callers that
    /// loaded the token for validation must use this rather than re-reading it, so
    /// the salt cannot disagree with the token the transaction was validated
    /// against.
    pub(crate) fn chain_tip_of(token: &pneumatic_core::tokens::Token) -> Vec<u8> {
        token.blockchain.get_current_chain_state().last_hash_in
    }

    /// Handle a "Process" request — a new transaction entering the pipeline.
    ///
    /// Flow:
    /// 1. Deserialize the transaction from the message body
    /// 2. Basic validation (sender present, nonce > 0)
    /// 3. Register in PendingTransactionRegistry as Pending
    /// 4. Preload data (users, token, contract) from DataProvider
    /// 5. Run spec-based validation
    /// 6. If self-signed (SelfSignedBlockValidatorSpec): skip to Committer
    /// 7. Otherwise: route to Executor for execution, then Finalizer
    pub(crate) fn handle_process_request(&self, message: Message) -> Result<(), SentinelError> {
        let tx: Transaction = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // Phase 3.1 (AUDIT finding C3): fail-closed sender authentication. This runs
        // *before* `compute_gas_used`/`validate_transaction` so a forged or unauthorized
        // transaction is dropped the instant it arrives, without touching the pool or
        // consuming gas. Two independent checks must both pass:
        //
        //   1. `tx.sender` must be non-empty.
        //   2. `tx.sender`'s Ed25519 signature over the canonical transaction bytes
        //      (`tx.sender_signature`) must verify — the signature proves the submitter
        //      actually authorized *this* payload, not a swapped replacement.
        //   3. The authenticated envelope sender (`message.public_key`, verified by the
        //      gossiper) must equal `tx.sender` — the network submitter must be the
        //      account being debited, so a peer can't debit an account it does not own.
        if tx.sender.is_empty() {
            return Err(SentinelError::UnauthenticatedSubmitter(
                "transaction has an empty sender".to_string(),
            ));
        }
        let sender_authorized = tx.verify_sender_signature().map_err(|e| {
            // `check_signature` returns Err on malformed/corrupt bytes; fail closed.
            SentinelError::InvalidSenderSignature(format!("sender-signature check error: {e}"))
        })?;
        if !sender_authorized {
            return Err(SentinelError::InvalidSenderSignature(
                "sender did not authorize this transaction".to_string(),
            ));
        }
        if message.public_key != tx.sender {
            return Err(SentinelError::UnauthenticatedSubmitter(
                "authenticated envelope sender does not match transaction sender".to_string(),
            ));
        }

        // Step 1: Compute gas used for this transaction (before validation, since validation may fail and we don't track gas for failed txs)
        let gas_used = self.transaction_validator.compute_gas_used(&tx);

        // Step 2: Basic validation
        if let Err(errors) = self.transaction_validator.validate_transaction(&tx, &message) {
            self.transition_to_failed(&tx.id, tx.clone(), errors);
            return Ok(());
        }

        // Step 3: Record gas used
        self.registry.record_gas_used(&tx.id, gas_used);

        // Step 2: Register transaction in the pending registry
        let tx_id = tx.id.clone();
        if self.registry.register_pending(tx_id.clone()).is_err() {
            return Err(SentinelError::TransactionAlreadyExists(tx_id));
        }

        // Step 3: Acquire lock for the preloading stage
        if self.registry.acquire_transaction(&tx_id).is_err() {
            return Err(SentinelError::TransactionInTerminalState(tx_id));
        }

        // Step 4: Route on the token's self-validation flag (AUDIT 5.9). Load the
        // token by tx.token_id and read its `is_self_verified` flag — the genuine
        // discriminator for a self-validated (owner-operated) token. Such a token is
        // governed by the `SelfSigned` spec (its only gate is `sender == owner`) and
        // skips Executor + Finalizer. Contract and other tokens default
        // `block_validation_spec_name` to "SelfSigned" but keep
        // `is_self_verified = false`, so routing on the flag keeps them on the
        // standard pipeline.
        let token = self
            .data_provider
            .get_token(&tx.token_id, &self.env_data.token_partition_id)
            .map_err(SentinelError::from)?;

        if token.is_self_verified {
            // Step 5: Self-validated token — skip Executor/Finalizer, route toward
            // commitment directly.
            return self.handle_self_signed(tx, gas_used);
        }

        // Step 6: Transition to Validated and enqueue into the pool for leader ordering.
        // Reject a replayed nonce (Phase 5.6 / H14) instead of silently admitting it.
        let risk = self.transaction_validator.calculate_risk(&tx);
        self.registry.transition_to_validated_and_enqueue(
            &tx_id,
            tx.clone(),
            pneumatic_core::transactions::TransactionValidationResult {
                is_valid: true,
                risk,
                failure_reasons: vec![],
                finalizer_public_key: vec![],
            },
        )
        .map_err(|e| SentinelError::Registry(e.to_string()))?;

        // Step 7: Standard pipeline — send to Executor for preloading
        self.send_to_executor_for_preload(&tx)
    }

    /// Handle a self-signed token transaction — skip Executor and Finalizer,
    /// route directly toward commitment.
    pub(crate) fn handle_self_signed(&self, tx: Transaction, gas_used: u64) -> Result<(), SentinelError> {
        let tx_id = tx.id.clone();

        // Transition to Validated state and enqueue into the pool in one atomic operation.
        // Reject a replayed nonce (Phase 5.6 / H14) instead of silently admitting it.
        let risk = self.transaction_validator.calculate_risk(&tx);
        self.registry.transition_to_validated_and_enqueue(
            &tx_id,
            tx.clone(),
            pneumatic_core::transactions::TransactionValidationResult {
                is_valid: true,
                risk,
                failure_reasons: vec![],
                finalizer_public_key: vec![], // Empty — self-signed, no finalizer
            },
        )
        .map_err(|e| SentinelError::Registry(e.to_string()))?;

        // Record gas used for this self-signed transaction
        self.registry.record_gas_used(&tx_id, gas_used);

        // Release the pre-lock so the committer's leader batch-proposal loop can
        // pick the tx up from the ordered pool. Self-signed tokens have no
        // Executor/Finalizer and no dedicated committer notifier — delivery is the
        // shared pool, drained by the committer, exactly like the standard path.
        let _ = self.registry.release_transaction(&tx_id);

        Ok(())
    }

    /// Send a transaction to Executors for data preloading.
    /// When sharding is enabled (shard_count > 1), routes only to the shard's executors.
    pub(crate) fn send_to_executor_for_preload(&self, tx: &Transaction) -> Result<(), SentinelError> {
        if self.env_data.shard_count > 1 {
            // Shard-aware routing: only send to the selected shard's executors.
            // The salt is the tip of the chain THIS transaction extends.
            let chain_tip = self.selection_tip(&tx.token_id)?;
            let shard_executors = self.get_shard_executors(&tx.id, *self.current_epoch.lock(), &chain_tip)?;
            self.transaction_notifier
                .send_to_shard_executors_for_preload(tx, &shard_executors, &self.env_data)
                .map_err(Into::into)
        } else {
            // No sharding: broadcast to all executors (existing behavior)
            self.transaction_notifier
                .send_to_executors_for_preload(tx, &self.env_data)
                .map_err(Into::into)
        }
    }

    /// Get the executor public keys for the transaction's shard.
    ///
    /// `chain_tip` is the selection salt — the tip of the chain the transaction
    /// extends, resolved by the caller via [`Self::selection_tip`] or
    /// [`Self::chain_tip_of`]. It is a parameter rather than fetched here so that
    /// every caller is forced to say which chain it is selecting against, and so a
    /// test can drive both branches without a provider that has to guess the same
    /// thing the production provider guesses.
    pub(crate) fn get_shard_executors(
        &self,
        tx_id: &str,
        epoch_number: u64,
        chain_tip: &[u8],
    ) -> Result<Vec<Vec<u8>>, SentinelError> {
        let executors = self.executor_set_cache.get(epoch_number)
            .ok_or_else(|| SentinelError::Routing(format!("No executor set for epoch {}", epoch_number)))?;

        if executors.is_empty() {
            return Err(SentinelError::Routing("Executor set is empty".into()));
        }

        let shard_executors = pneumatic_core::deterministic_select_shard(
            &executors,
            self.env_data.shard_count,
            tx_id,
            epoch_number,
            chain_tip,
        )
        .ok_or_else(|| SentinelError::Routing("Selected shard has no executors".into()))?;

        if shard_executors.is_empty() {
            return Err(SentinelError::Routing("Selected shard has no executors".into()));
        }

        Ok(shard_executors)
    }

    /// Handle a "Clear"/"Delete" request — remove a transaction from the registry.
    pub(crate) fn handle_clear_request(&self, message: Message) -> Result<(), SentinelError> {
        let tx_id: String = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        let _ = self.registry.remove_transaction(&tx_id);
        Ok(())
    }

    /// Transition a transaction to Failed state with error reasons.
    pub(crate) fn transition_to_failed(&self, tx_id: &str, tx: Transaction, error: PneumaticError) {
        match error {
            PneumaticError::Validation(reasons) => {
                if let Ok(mut entry) = self.registry.get_transaction_mut(tx_id) {
                    entry.transition_to_failed(tx, reasons);
                }
            }
            _ => {
                if let Ok(mut entry) = self.registry.get_transaction_mut(tx_id) {
                    entry.transition_to_failed(tx, vec![]);
                }
            }
        }
    }

}
