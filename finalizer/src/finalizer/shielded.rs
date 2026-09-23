//! Shielded-transfer concern for the finalizer: C1 auth for the shielded
//! sign/vote messages, advisory `validate_shielded`, shielded vote handling,
//! and shielded finalization.

use super::*;

impl Finalizer {
/// Authenticate an inbound shielded (vote request or vote) message.
///
/// The S5.2 C1 pattern for the shielded arms: the envelope signature must
/// verify against the claimed key, and that key must be registered with
/// the `Finalizer` role (the Executor stage is skipped for shielded
/// transfers — the sender of a `SignShielded` vote request is the
/// composite node's own finalizer identity; a split deployment registers
/// its voting finalizer as a Finalizer). Unknown / role-less signers are
/// rejected fail-closed before any other work.
fn authenticate_shielded_message(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
    // (1) Envelope signature: verified against the claimed public key.
    //     `check_signature` is pure and returns `Ok(false)` (never panics)
    //     on malformed input.
    if !self
        .identity
        .ed25519
        .check_signature(&message.signature, &message.public_key, &message.body)
        .map_err(|e| {
            PneumaticError::CryptoError(format!(
                "envelope signature verification failed for {}: {e}",
                bytes_to_hex(&message.public_key)
            ))
        })?
    {
        return Err(PneumaticError::CryptoError(format!(
            "envelope signature verification failed for {}",
            bytes_to_hex(&message.public_key)
        )));
    }

    // (2) Role gate: the verified signer must be registered as a
    //     `Finalizer` (among its full role set — a composite voter
    //     registered as Finalizer + other roles still authenticates).
    let roles = self.node_registry.find_node_types_by_public_key(&message.public_key);
    match roles.is_empty() {
        false if roles.contains(&NodeRegistryType::Finalizer) => Ok(message.public_key.clone()),
        false => Err(PneumaticError::Registry(format!(
            "sender {} is registered as {:?}, not a Finalizer",
            bytes_to_hex(&message.public_key),
            roles
        ))),
        true => Err(PneumaticError::Registry(format!(
            "sender {} is not registered as any node",
            bytes_to_hex(&message.public_key)
        ))),
    }
}

/// Re-run the `Shielded` validation spec against the finalizer's **own**
/// pool view (S5.1 contract: the same `validate_shielded` call the sentinel
/// ran, against this node's state — the sentinel's verdict is never
/// relayed). Advisory: it gates *this* finalizer's vote, not consensus —
/// the committer re-runs the spec authoritatively at commit time (S5.3).
/// Any failure → no vote, no state mutation.
fn validate_shielded_advisory(&self, stx: &ShieldedTransaction) -> Result<(), PneumaticError> {
    let spec = self
        .env_data
        .transaction_validation_specs
        .get(ShieldedValidationSpec::NAME)
        .ok_or_else(|| {
            PneumaticError::Validation(vec![ValidationFailureReason::UnsupportedAction])
        })?;
    let deps = ShieldedValidationDeps {
        spent: self.pool_view.nullifier_set(),
        roots: self.pool_view.root_history(),
        recency_window: self.env_data.shielded_root_recency,
    };
    spec.validate_shielded(stx, &self.env_data, &deps).map(|_| ())
}

/// Handle a `"SignShielded"` vote request from the Sentinel (Phase S5.2 —
/// the real path; the S3.2 stub is retired).
///
/// Fail-closed steps, in this order:
/// 1. Authenticate the envelope sender as a registered Finalizer (C1) —
///    unknown / role-less voter → reject, no vote.
/// 2. Canonical-rmp deserialize the `ShieldedTransaction` body — a
///    malformed body is an `Encoding` error before any other work.
/// 3. Re-run `validate_shielded` against this finalizer's **own** pool
///    view (advisory — see `validate_shielded_advisory`).
/// 4. Record the canonical bytes (idempotent for a byte-identical
///    resubmission; same id with *different* bytes is a conflict →
///    reject, no vote) so `try_finalize_shielded` rebuilds the exact
///    payload the vote binds to.
/// 5. Sign `stx.hash()` (SHA-256 over the canonical bytes — binds
///    nullifiers, commitments, referenced pre root, proof, ciphertexts,
///    tx id) and fan the `ShieldedVote` out to the finalizer peers.
pub async fn handle_sign_shielded(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
    // (1) C1 authentication, Finalizer role.
    let sender_key = self.authenticate_shielded_message(message)?;

    // (2) Canonical-byte deserialization.
    let stx: ShieldedTransaction =
        deserialize_rmp_to(&message.body).map_err(|e| PneumaticError::Encoding(e.to_string()))?;
    let canonical = stx.canonical_bytes()?;

    // (3) Advisory re-verification against this node's own view.
    self.validate_shielded_advisory(&stx)?;

    // (4) Record the exact bytes (fail closed on a same-id conflict).
    {
        let mut store = self.shielded_transactions.lock().await;
        match store.get(&stx.id) {
            Some(existing) if *existing != canonical => {
                return Err(PneumaticError::Registry(format!(
                    "conflicting shielded tx for id {}: same id, different canonical bytes",
                    stx.id
                )));
            }
            _ => {
                store.insert(stx.id.clone(), canonical);
            }
        }
    }

    // (5) Sign the pre-root hash over the canonical bytes and fan out the
    //     vote. The stake is stamped from the epoch snapshot — never
    //     self-reported.
    let tx_hash = stx.hash()?;
    let signature = self
        .identity
        .ed25519
        .sign_data(&tx_hash)
        .map_err(|e| PneumaticError::CryptoError(format!("shielded vote signing failed: {e}")))?;
    let vote = TransactionSignature {
        transaction_id: stx.id.as_bytes().to_vec(),
        env_id: self.env_id.clone().into_bytes(),
        transaction_hash: tx_hash,
        signature,
        current_stake: self.current_stake_for_voter(&sender_key),
    };
    self.message_dispatcher
        .send_shielded_vote_to_finalizers(vote)
        .await?;

    Ok(pneumatic_core::messages::acknowledge())
}

/// Handle a `"ShieldedVote"` vote from a voting finalizer to the collector
/// (Phase S5.2 — the real path; the S3.2 stub is retired).
///
/// Fail-closed steps, in this order:
/// 1. Authenticate the envelope sender as a registered Finalizer (C1).
/// 2. Deserialize the `TransactionSignature` vote (the existing wire
///    struct — no new shape).
/// 3. Verify the inner vote signature over the claimed `transaction_hash`
///    (negated-verify discipline — a bare `?` would accept a failing
///    verification).
/// 4. Stamp `current_stake` from the epoch snapshot (never
///    self-reported), then feed the collector — a duplicate vote is
///    rejected without state mutation.
/// 5. Stake-weighted quorum (exact-integer `u128`, **no optimistic fast
///    path** — shielded txs commit only at stake quorum, roadmap 2.1).
///    Quorum met → `try_finalize_shielded`.
pub async fn handle_shielded_vote(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
    // (1) C1 authentication, Finalizer role.
    let voter_key = self.authenticate_shielded_message(message)?;

    // (2) Deserialize the vote.
    let mut vote: TransactionSignature =
        deserialize_rmp_to(&message.body).map_err(|e| PneumaticError::Encoding(e.to_string()))?;

    // (3) Verify the inner signature over the claimed transaction hash.
    if !self
        .identity
        .ed25519
        .check_signature(&vote.signature, &voter_key, &vote.transaction_hash)
        .map_err(|e| {
            PneumaticError::CryptoError(format!(
                "shielded vote signature verification failed for {}: {e}",
                bytes_to_hex(&voter_key)
            ))
        })?
    {
        return Err(PneumaticError::CryptoError(format!(
            "shielded vote signature verification failed for {}",
            bytes_to_hex(&voter_key)
        )));
    }

    // (4) Stamp real stake from the snapshot; collect (duplicate → reject).
    vote.current_stake = self.current_stake_for_voter(&voter_key);
    let tx_id = String::from_utf8_lossy(&vote.transaction_id).to_string();
    self.signature_collector
        .add_signature(&tx_id, voter_key.clone(), vote.clone())?;

    // (5) Stake-weighted quorum — no optimistic branch.
    let (total_stake, _total_voters) = self.resolve_stake_metrics();
    if self
        .signature_collector
        .check_stake_quorum(&tx_id, total_stake)?
    {
        return self.try_finalize_shielded(&tx_id).await;
    }

    Ok(pneumatic_core::messages::acknowledge())
}

/// Finalize a shielded transfer at stake-weighted quorum (the S5.2
/// shielded tail — a new sibling of `try_finalize`, which is inapplicable:
/// shielded txs live in the parallel recorded-bytes store, not the
/// `Pending → … → Committed` state machine).
///
/// 1. Reconcile the collected votes.
/// 2. Rebuild the `ShieldedTransaction` from the recorded canonical bytes
///    (the exact payload the votes were signed over).
/// 3. Integrity: every collected vote must bind `stx.hash()` of the
///    recorded bytes — a vote whose claimed hash differs (e.g. a tx
///    mutated after signing) aborts finalization fail-closed.
/// 4. `build_signed_transaction_shielded` — the S3.1 placeholder
///    `Transaction`, `shielded: Some(stx)`, and the votes in
///    `executor_sigs` (keyed by voting finalizer pubkeys; the map is the
///    canonical voter-sig carrier in shielded blocks) → `sign_finalizer_block`
///    (unchanged formula — `rmp(signed_tx)` already covers `shielded`)
///    → `create_block`.
/// 5. `TransactionCommit` → `send_to_committers` + `send_block_finalized`
///    (the existing committer/sentinel tail — no clear-to-sentinels: the
///    sentinel's shielded map is keyed by the tx id and cleaned by its own
///    commit-ack path, S5.3).
/// 6. Clean up the recorded bytes + signature registry entry.
async fn try_finalize_shielded(&self, tx_id: &str) -> Result<Vec<u8>, PneumaticError> {
    // 1. Reconcile the collected votes.
    let reconciled = self.signature_collector.reconcile_signatures(tx_id)?;

    // 2. Rebuild the recorded shielded tx from its canonical bytes.
    let stx_bytes = self
        .shielded_transactions
        .lock()
        .await
        .get(tx_id)
        .ok_or_else(|| {
            PneumaticError::Registry(format!(
                "no recorded shielded tx for {tx_id} — cannot finalize without the exact bytes"
            ))
        })?
        .clone();
    let stx: ShieldedTransaction =
        deserialize_rmp_to(&stx_bytes).map_err(|e| PneumaticError::Encoding(e.to_string()))?;

    // 3. Every collected vote must bind the recorded bytes' hash.
    let stx_hash = stx.hash()?;
    if let Some(sig_map) = self.signature_registry.get_transaction_registry(tx_id) {
        for (voter_key, vote) in sig_map.iter() {
            if vote.transaction_hash != stx_hash {
                return Err(PneumaticError::CryptoError(format!(
                    "shielded vote from {} binds a hash different from the recorded tx for {tx_id} — rejecting finalization",
                    bytes_to_hex(voter_key)
                )));
            }
        }
    }

    // 4. Build the signed transaction (votes in `executor_sigs`) and sign.
    let (total_stake, total_voters) = self.resolve_stake_metrics();
    let mut signed_tx = self.block_builder.build_signed_transaction_shielded(
        &reconciled,
        &stx,
        total_stake,
        total_voters,
    );
    let finalizer_sig = self.block_builder.sign_finalizer_block(&mut signed_tx).await?;
    signed_tx.finalizer_sig = finalizer_sig;

    // 5. Create the block and dispatch the commit.
    let previous_hash = self.resolve_previous_hash(&stx.token_id);
    let block =
        self.block_builder.create_block(signed_tx.clone(), previous_hash, self.current_epoch)?;
    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: stx.token_id.clone(),
        env_id: self.env_id.clone(),
        // Clone for the commit; the moved `block` goes out in the
        // BlockFinalized broadcast immediately after.
        proposed_block: block.clone(),
    };
    self.message_dispatcher.send_to_committers(commit).await?;
    self.message_dispatcher
        .send_block_finalized(block, self.get_stake_set_for_epoch())
        .await?;

    // 6. Clean up the recorded bytes + the signature registry entry.
    self.shielded_transactions.lock().await.remove(tx_id);
    let _ = self.signature_registry.try_remove_transaction(tx_id);

    Ok(
        serialize_to_bytes_rmp(&tx_id.as_bytes().to_vec())
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?,
    )
}
}
