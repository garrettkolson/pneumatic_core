//! Signature-intake concern for the finalizer: the C1 voter-auth
//! chokepoint (envelope + inner signature, registered-Executor gate), voter
//! stake from the epoch snapshot (never the message), preload, and the `Sign`
//! handler feeding the signature collector and quorum gate.

use super::*;
use pneumatic_core::auth::{authenticate_envelope, EnvelopeAuthError};

impl Finalizer {
/// Authenticate an inbound voter (executor) message.
///
/// Implements the finalizer side of audit finding C1: the voter's identity is
/// the public key *proven* by the envelope signature, never the self-declared
/// `message.public_key` used for routing. Returns the voter's public key on
/// success, or `PneumaticError` (fail closed) if the envelope signature does
/// not verify or the key is not registered as an `Executor`.
///
/// This is the single chokepoint every voter signature passes through
/// (`handle_signature`), so the registered-`Executor` requirement here
/// prevents any unregistered key from ever entering the signature registry.
fn authenticate_signature_message(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
    // (1) Envelope signature + registration resolution: the shared core C1
    //     primitive (signature over `body` verified against the claimed
    //     `public_key` — including the `Ok(false)`-means-mismatch trap —
    //     then the registry lookup resolving the sender's full role set).
    let roles = authenticate_envelope(
        &self.identity.ed25519,
        &self.node_registry,
        &message.signature,
        &message.public_key,
        &message.body,
    )
    .map_err(|e| match e {
        EnvelopeAuthError::Signature {
            reason: Some(reason),
            ..
        } => PneumaticError::CryptoError(format!(
            "envelope signature verification failed for {}: {reason}",
            bytes_to_hex(&message.public_key)
        )),
        EnvelopeAuthError::Signature { .. } => PneumaticError::CryptoError(format!(
            "envelope signature verification failed for {}",
            bytes_to_hex(&message.public_key)
        )),
        EnvelopeAuthError::Unregistered { .. } => PneumaticError::Registry(format!(
            "sender {} is not registered as any node",
            bytes_to_hex(&message.public_key)
        )),
    })?;

    // (2) Role gate (local policy): the verified signer must be registered as
    // an `Executor` — among its full role set (Phase 6), so a composite voter
    // registered as Executor (and other roles) still authenticates, while a
    // signer with no Executor role is rejected (fail closed).
    if roles.contains(&NodeRegistryType::Executor) {
        Ok(message.public_key.clone())
    } else {
        Err(PneumaticError::Registry(format!(
            "sender {} is registered as {:?}, not an Executor",
            bytes_to_hex(&message.public_key),
            roles
        )))
    }
}

/// Resolve a voter's stake from the current epoch's snapshot.
///
/// Returns the voter's recorded stake, or `0` if the snapshot is unavailable
/// or the voter has no recorded stake. Used to stamp `current_stake` on an
/// admitted signature so stake-weighted reconciliation can never trust a
/// self-reported stake from the message.
pub(crate) fn current_stake_for_voter(&self, voter_pubkey: &[u8]) -> u64 {
    match self.get_stake_set_for_epoch() {
        Some(set) => set.stakers.get(voter_pubkey).copied().unwrap_or(0),
        None => 0,
    }
}

/// Handle a Preload message from the Sentinel/Executor.
///
/// Receives preloaded transaction data and stores it for later processing.
/// Returns an acknowledgement message.
pub async fn handle_preload(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
    // Deserialize the transaction payload
    let tx: Transaction = deserialize_rmp_to(&message.body)
        .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

    // ADR-004: register only what this node was chosen to finalize.
    //
    // Refusing here is the cheapest place to stop a fork, because everything
    // downstream of a preload reads from this registry: a Finalizer that stores a
    // transaction assigned elsewhere goes on to build a block for it the moment its
    // executor's vote arrives. On a composite this also closes the shared-registry
    // route, where the entry is already present because *this host's own Sentinel
    // role* registered it — the refresh path below is exactly how a not-mine
    // transaction would otherwise be adopted.
    //
    // Note the asymmetry with the envelope: preload carries no sender
    // authentication (see below, and the C1 scoping note), so this is not a
    // "who sent it" check. It is a "is this mine" check, and it holds regardless of
    // who delivered it — a forged or misrouted preload names a Finalizer, and if
    // that is not this node, nothing happens here.
    self.finalize_authority(&tx)?;

    // Store as a preload task. We persist the *serialized transaction*, not
    // the envelope signature — after Phase 1.1 `message.signature` is a live
    // 64-byte Ed25519 signature, and storing it here would masquerade as
    // preload payload. The sender of preload is Sentinel/Executor and is
    // out of scope for C1 (envelope auth is NOT added to handle_preload in
    // this phase).
    self.preload_tasks
        .lock()
        .await
        .insert(tx.id.clone(), serialize_to_bytes_rmp(&tx)?);

    // Register the transaction in this finalizer's own pending registry,
    // because the optimistic-finality path (`try_finalize_optimistic`) loads
    // the transaction from `self.pending_registry` and fails closed if the
    // entry is missing. The entry must be in an executable state
    // (Preloaded/Validated/Executing), so a fresh Pending entry is transitioned
    // to Preloaded with the deserialized transaction.
    if self.pending_registry.contains(&tx.id) {
        if let Ok(mut entry) = self.pending_registry.get_transaction_mut(&tx.id) {
            match &entry.state {
                TransactionState::Pending => {
                    entry.transition_to_preloaded(tx.clone());
                }
                // P10 (composite shared registry): the preloaded tx is the
                // executor's stamped view (`result_hash` + `result_data` set)
                // — newer than the sentinel-registered copy. Refresh the
                // canonical entry so the wire block the finalizer commits and
                // the registry entry the committer hash-matches (H12) carry
                // the identical transaction. Terminal states are never
                // overwritten.
                TransactionState::Preloaded { .. }
                | TransactionState::Validated { .. }
                | TransactionState::Executing { .. } => {
                    let state = match &entry.state {
                        TransactionState::Validated { validation, .. } => {
                            TransactionState::Validated {
                                transaction: tx.clone(),
                                validation: validation.clone(),
                            }
                        }
                        _ => TransactionState::Preloaded {
                            transaction: tx.clone(),
                        },
                    };
                    entry.state = state;
                }
                _ => {}
            }
        }
    } else {
        self.pending_registry
            .register_pending(tx.id.clone())
            .map_err(|e| PneumaticError::Registry(format!("preload register: {e}")))?;
        if let Ok(mut entry) = self.pending_registry.get_transaction_mut(&tx.id) {
            entry.transition_to_preloaded(tx.clone());
        }
    }

    // This transaction may already hold a vote. A preload that lands after its own
    // executor's "Sign" is not a rare interleaving — it is what a transport that
    // acknowledges on receipt plus a task-spawning bridge produces, and it happened on
    // every frame of the RNS pipeline e2e. Registering the entry is only half of
    // catching up: the optimistic attempt that ran without one has to run again.
    self.retry_finalize_from_collected(&tx.id).await;

    // Acknowledge receipt
    Ok(pneumatic_core::messages::acknowledge())
}

/// Handle a Signature message from an Executor.
///
/// OPTIMISTIC: First valid signature triggers immediate optimistic finalize.
/// Subsequent signatures accumulate stake and are acknowledged.
/// If quorum is eventually reached, the transaction is upgraded to confirmed.
///
/// Returns an acknowledgement if added, or the result of optimistic finalize
/// if this signature completed the optimistic path.
pub async fn handle_signature(&self, message: &Message) -> Result<Vec<u8>, PneumaticError> {
    // Deserialize the executor signature
    let mut sig: TransactionSignature = deserialize_rmp_to(&message.body)
        .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

    // Extract transaction ID from the signature
    let tx_id = String::from_utf8_lossy(&sig.transaction_id).to_string();

    // (C1) Authenticate the voter. The key that enters the signature registry —
    // and that the optimistic path credits — is the public key proven by the
    // envelope signature and confirmed as a registered `Executor`. Fails
    // closed on any anomaly.
    let voter_pubkey = self.authenticate_signature_message(message)?;

    // (C1) Verify the inner signature actually signs this voter's claimed
    // transaction hash with their key. Rejects any voter whose claimed
    // signature does not verify over the claimed hash. Mirrors the envelope
    // check above: `check_signature` returns `Ok(false)` on a mismatch, so
    // the result must be negated (a bare `?` would only propagate a provider
    // error and silently accept a failing verification).
    if !self
        .identity
        .ed25519
        .check_signature(&sig.signature, &voter_pubkey, &sig.transaction_hash)
        .map_err(|e| {
            PneumaticError::CryptoError(format!(
                "executor signature verification failed for {}: {e}",
                bytes_to_hex(&voter_pubkey)
            ))
        })?
    {
        return Err(PneumaticError::CryptoError(format!(
            "executor signature verification failed for {}",
            bytes_to_hex(&voter_pubkey)
        )));
    }

    // (C1) Stamp the voter's real stake from the epoch snapshot — never the
    // self-reported stake carried in the message.
    sig.current_stake = self.current_stake_for_voter(&voter_pubkey);

    // ADR-004: a vote is only actionable by the Finalizer the transaction was
    // routed to. Checking before the collector, not just before the block, keeps a
    // transaction assigned elsewhere out of this node's signature registry at all —
    // otherwise every host in the epoch accumulates votes for work it does not own,
    // and the difference only shows up as a second block for one transaction.
    self.finalize_authority_for_id(&tx_id)?;

    // Add the signature to the collector. This is the only path a key enters
    // the registry, and the key is already authenticated.
    self.signature_collector
        .add_signature(&tx_id, voter_pubkey.clone(), sig.clone())?;

    // OPTIMISTIC: First valid signature → try optimistic finalize immediately.
    // The signer is now authenticated + registered, so an attacker cannot
    // forge a registered executor's signature to trigger an optimistic commit.
    if self.signature_collector.signature_count(&tx_id) == 1 {
        return self
            .try_finalize_optimistic(&tx_id, &sig, &voter_pubkey)
            .await;
    }

    // Subsequent signatures — just acknowledge (stake accumulates in background)
    // If quorum is eventually reached, the transaction will be confirmed
    Ok(pneumatic_core::messages::acknowledge())
}
}
