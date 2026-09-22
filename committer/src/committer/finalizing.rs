//! Production commit-path logic for the Committer, extracted from
//! `crate::committer` to keep that module focused on routing and construction.
//!
//! These are `impl Committer` methods, so they retain access to the struct's
//! private fields (child modules of `crate::committer`).

use super::*;

impl Committer {

    /// Handle "BlockFinalized" gossip from the finalizer.
    ///
    /// This is the entry point for quorum gossip — the finalizer has
    /// committed a block and is broadcasting it to all nodes.
    /// Receivers validate the block, append it, and broadcast a vote.
    /// Fail-closed finalizer-signature check (AUDIT Phase 3.3 / C5).
    ///
    /// A `BlockFinalized` block carries exactly one authoritative signature — the finalizer's, in
    /// `SignedTransaction.finalizer_sig`. Its `transaction_hash` is the hash the finalizer actually
    /// signed, so the Ed25519 verification needs no reconstruction. Because `create_hash` binds the
    /// whole `finalizer_sig` into the block hash (canonical serialization includes it), a swapped
    /// signature would already fail the linkage/hash check in `append_validated_block`. Reject a
    /// missing or unverified signature rather than silently accepting it.
    pub(crate) fn verify_block_finalizer_sig(&self, block: &Block) -> Result<(), CommitterError> {
        let finalizer_sig = &block.signed_trans.finalizer_sig;
        if finalizer_sig.signature.is_empty() {
            return Err(CommitterError::InvalidFinalizerSignature);
        }
        let valid = self.identity.ed25519.check_signature(
            &finalizer_sig.signature,
            &block.signed_trans.finalizer_addr,
            &finalizer_sig.transaction_hash,
        )?;
        if !valid {
            return Err(CommitterError::InvalidFinalizerSignature);
        }
        Ok(())
    }

    pub(crate) async fn handle_block_finalized(&self, message: Message) -> Result<(), CommitterError> {
        let block: Block = deserialize_rmp_to(&message.body)
            .map_err(CommitterError::Deserialization)?;

        let block_hash = block.current_hash.clone();
        let token_id = &block.signed_trans.transaction.token_id;

        // Cache the stake set for later vote processing
        if let Some(ref stake_set) = message.stake_set {
            let mut cache = self.stake_set_cache.lock().await;
            cache.insert(block_hash.clone(), stake_set.clone());
        }

        // Fail-closed finalizer-signature check (AUDIT Phase 3.3 / C5): reject a block whose
        // finalizer signature does not verify. Pure computation over the block's own fields — no
        // lock is held here.
        self.verify_block_finalizer_sig(&block)?;

        // Validate linkage + hash and append under a SINGLE mutable borrow of the token (AUDIT
        // Phase 3.3 / C5). This closes the read-then-`get_mut` gap: previously the tip was read via
        // an immutable borrow, dropped, and only then re-looked-up mutably — so two concurrent
        // sibling blocks could both validate and both append. `append_validated_block` reads the tip
        // and appends inside one `&mut self`, which maps here to a single `get_mut` on the token.
        // Blocks committed by this call — the original append plus any promoted from the orphan
        // buffer. Each is distributed to archivars and voted on, exactly as in the plain path.
        let mut committed: Vec<Block> = Vec::new();

        // Validate linkage + hash and append under a SINGLE mutable borrow of the token (AUDIT
        // Phase 3.3 / C5). This closes the read-then-`get_mut` gap: previously the tip was read via
        // an immutable borrow, dropped, and only then re-looked-up mutably — so two concurrent
        // sibling blocks could both validate and both append. `append_validated_block` reads the tip
        // and appends inside one `&mut self`, which maps here to a single `get_mut` on the token.
        {
            let mut entry = self.tokens.get_mut(token_id).ok_or_else(|| {
                CommitterError::TokenNotFound(bytes_to_hex(token_id))
            })?;
            match entry.value_mut().blockchain.append_validated_block(&block) {
                AppendOutcome::LinkageMismatch => {
                    // AUDIT Phase 3.4 / H15: the receiver is behind — this is the next block in a
                    // sequence whose parent has not yet landed, NOT a sibling competitor. Buffer it
                    // and replay it as the tip advances instead of silently dropping it.
                    self.buffer_orphan(token_id.clone(), block).await;
                    return Ok(());
                }
                AppendOutcome::InvalidHash => {
                    self.env_data.logger.log(format!(
                        "BlockFinalized: invalid hash for token [{}], rejecting",
                        bytes_to_hex(token_id)
                    ));
                    return Err(CommitterError::InvalidBlockHash);
                }
                AppendOutcome::Appended => {
                    committed.push(block.clone());
                }
            }
        }

        // AUDIT Phase 3.4 / H15: the tip just advanced by `block_hash`. Replay any buffered blocks
        // whose parent is now the tip, cascading as the promoted chain grows.
        let promoted = self.replay_orphan_blocks(&block_hash, token_id).await;
        committed.extend(promoted);

        // Propagate every block we committed this call (the original plus the promoted ones).
        for committed_block in &committed {
            // Distribute to archivars (propagate gossip)
            let _ = self.block_services.distribute_to_archivers(committed_block).await;

            // Broadcast our own vote: we've received and validated this block
            self.broadcast_vote(&committed_block.current_hash).await;
        }

        Ok(())
    }

    /// Buffer an out-of-order finalized block for later replay (AUDIT Phase 3.4 / H15).
    ///
    /// Called when a BlockFinalized's block does not chain onto the current tip. The block is held
    /// in the orphan buffer keyed by token; whether it was buffered — or dropped because the buffer
    /// is full — is always logged, so the drop is observable, never silent.
    pub(crate) async fn buffer_orphan(&self, token_id: Vec<u8>, block: Block) {
        let mut orphan_blocks = self.orphan_blocks.lock().await;
        // Compute the token hex before `token_id` is moved into `insert` below.
        let token_hex = bytes_to_hex(&token_id);
        match orphan_blocks.insert(token_id, block) {
            BufferDecision::Buffered => {
                self.env_data.logger.log(format!(
                    "BlockFinalized: buffered out-of-order block for token [{token_hex}] for replay"
                ));
            }
            BufferDecision::RejectedFull => {
                self.env_data.logger.log(format!(
                    "BlockFinalized: orphan buffer full for token [{token_hex}], dropping out-of-order block"
                ));
            }
        }
    }

    /// Promote buffered blocks whose parent hash is `tip_hash`, cascading to blocks whose parent
    /// chains onto each promoted block (AUDIT Phase 3.4 / H15).
    ///
    /// A candidate is selected under the orphan lock only, then appended under a single `get_mut`
    /// on the token (the same atomic read-tip-then-append shape as the plain path), so a promoted
    /// append cannot race a concurrent handler and there is no nested lock. Returns the promoted
    /// blocks in commit order.
    pub(crate) async fn replay_orphan_blocks(
        &self,
        tip_hash: &[u8],
        token_id: &[u8],
    ) -> Vec<Block> {
        let mut committed = Vec::new();
        let mut expected_tip = tip_hash.to_vec();

        loop {
            // Select the next buffered block that chains onto `expected_tip`, removing it from the
            // buffer so it is "in flight" even if eviction or a concurrent handler touches it next.
            let chosen = {
                let mut orphan_blocks = self.orphan_blocks.lock().await;
                orphan_blocks.drop_expired(Instant::now());
                orphan_blocks.take_matching(token_id, &expected_tip, Instant::now())
            };

            let chosen = match chosen {
                Some(chosen) => chosen,
                None => break,
            };

            // Append atomically. The orphan lock is released before the token lock is taken, so
            // there is no new lock-ordering hazard.
            let append_outcome = {
                match self.tokens.get_mut(token_id) {
                    Some(mut entry) => {
                        entry.value_mut().blockchain.append_validated_block(&chosen)
                    }
                    None => break,
                }
            };

            match append_outcome {
                AppendOutcome::Appended => {
                    committed.push(chosen.clone());
                    // The promoted block's own hash is now the tip — keep cascading.
                    expected_tip = chosen.current_hash.clone();
                }
                AppendOutcome::InvalidHash => {
                    // A buffered block that is now internally inconsistent (tampered) — fail
                    // closed and stop the cascade.
                    self.env_data.logger.log(format!(
                        "BlockFinalized: replayed block for token [{}] failed hash check, rejecting",
                        bytes_to_hex(token_id)
                    ));
                    break;
                }
                AppendOutcome::LinkageMismatch => {
                    // A sibling raced in and advanced the tip past `chosen`'s parent while we
                    // were selecting. Re-queue it so a later replay can retry if the tip returns.
                    let mut orphan_blocks = self.orphan_blocks.lock().await;
                    orphan_blocks.requeue_back(token_id, chosen);
                    break;
                }
            }
        }

        committed
    }
}
