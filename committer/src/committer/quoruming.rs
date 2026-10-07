//! Production commit-path logic for the Committer, extracted from
//! `crate::committer` to keep that module focused on routing and construction.
//!
//! These are `impl Committer` methods, so they retain access to the struct's
//! private fields (child modules of `crate::committer`).

use super::*;

impl Committer {
    /// Handle "BlockConfirmed" vote from peers.
    ///
    /// Each node that validates a BlockFinalized message broadcasts a vote.
    /// This handler accumulates votes and checks for quorum.
    pub(crate) async fn handle_block_confirmed_vote(&self, message: Message) -> Result<(), CommitterError> {
        // Deserialize vote: (block_hash, voter_public_key)
        let (block_hash, voter_key): (Vec<u8>, Vec<u8>) =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        self.cast_confirmation_vote(&block_hash, voter_key).await;
        self.announce_quorum_if_reached(&block_hash).await;
        Ok(())
    }

    /// Record a confirmation vote, buffering it when the block's stake set has
    /// not arrived yet so it is never lost.
    ///
    /// The old behaviour was `return Ok(())` on a missing stake set — a vote
    /// discarded with no counter and no log. Because a peer gossips its vote the
    /// moment it validates a block, racing ahead of the block's own propagation is
    /// the normal case rather than the rare one, so a committer could lose every
    /// vote it ever received and conclude that nothing had quorum, forever.
    pub(crate) async fn cast_confirmation_vote(&self, block_hash: &[u8], voter_key: Vec<u8>) {
        if self.accumulate_vote(block_hash, voter_key.clone()).await.is_some() {
            return;
        }
        self.buffered_confirmation_votes
            .fetch_add(1, Ordering::Relaxed);
        let mut pending = self.pending_confirmation_votes.lock().await;
        let entry = pending.entry(block_hash.to_vec()).or_insert_with(Vec::new);
        // One line per block rather than one per vote: early votes are the common
        // case, and the counter above carries the volume.
        if entry.is_empty() {
            self.env_data.logger.log(format!(
                "confirmation votes for block {} are arriving before its stake set; buffering",
                bytes_to_hex(block_hash)
            ));
        }
        entry.push(voter_key);
    }

    /// Add one voter's stake to a block's running total and return the new
    /// cumulative stake, or `None` when this block's stake set has not arrived.
    ///
    /// Two properties this carries, both absent before Phase 0 item 3:
    ///
    /// - **The denominator is declared, the numerator is earned.** The threshold
    ///   is the block's cached stake-set total, which arrives with the block; the
    ///   numerator only ever grows by stake looked up *from that same set*. A key
    ///   outside the set resolves to zero stake, so an unregistered voter cannot
    ///   inflate a quorum no matter how many votes it sends.
    /// - **A voter counts once.** Deduplicated by key, so a node holding several
    ///   roles (a composite identity) casts one vote, not one per role.
    ///
    /// This method deliberately does not skip `self.public_key`. The previous
    /// version returned early on it with the comment "we already voted via
    /// `handle_block_finalized`" — but that path only broadcast a vote over the
    /// network and never recorded it here, so a committer's own stake was in the
    /// denominator and could never be in the numerator. With equal stake that
    /// makes 3 committers unable to ever reach 67%: each sees 2 of 3, which is
    /// 66.7%, and `200 >= 201` is false. No block would ever become `Confirmed`,
    /// with nothing logged. Self is now recorded by `handle_block_finalized` and
    /// the key dedup above stops it being counted twice.
    async fn accumulate_vote(&self, block_hash: &[u8], voter_key: Vec<u8>) -> Option<u64> {
        let voting_stake = {
            let cache = self.stake_set_cache.lock().await;
            cache.get(block_hash)?.get_stake(&voter_key)
        };

        let mut votes = self.confirmation_votes.lock().await;
        let entry = votes
            .entry(block_hash.to_vec())
            .or_insert_with(|| (HashSet::new(), 0u64));

        if !entry.0.insert(voter_key) {
            // Already counted — a duplicate vote adds no stake.
            return Some(entry.1);
        }

        entry.1 = entry.1.saturating_add(voting_stake);
        Some(entry.1)
    }

    /// This committer's own verdict on whether a block has reached quorum.
    ///
    /// `None` means *cannot judge yet* — no stake set or no votes recorded. That
    /// is distinct from `Some(false)`, which is a considered "not yet": a claim
    /// rejected on `Some(false)` is a discrepancy worth counting, while `None` is
    /// simply early.
    pub(crate) async fn local_quorum_reached(&self, block_hash: &[u8]) -> Option<bool> {
        let total = {
            let cache = self.stake_set_cache.lock().await;
            cache.get(block_hash)?.total_stake()
        };
        if total == 0 {
            return Some(false);
        }
        let cumulative = {
            let votes = self.confirmation_votes.lock().await;
            votes.get(block_hash)?.1
        };

        // AUDIT Phase 6.9 (Item B, discriminator): exact-integer quorum. Casting
        // u64 stake to f64 truncates above 2^52, so the f64 threshold can be off by
        // one at the boundary — an adversarial stake could reach or miss quorum
        // wrongly. cumulative/total >= quorum/100  <=>  cumulative*100 >= total*quorum,
        // all in u128 (u64::MAX * 100 fits in u128). quorum_percentage is f32
        // validated to (0,100]; round() to nearest whole percent.
        let quorum_pct = self.env_data.quorum_percentage.round() as u128;
        Some((cumulative as u128) * 100 >= (total as u128) * quorum_pct)
    }

    /// Broadcast `BlockQuorumReached` if — and only if — this node's own
    /// arithmetic says the block has quorum.
    pub(crate) async fn announce_quorum_if_reached(&self, block_hash: &[u8]) {
        if self.local_quorum_reached(block_hash).await == Some(true) {
            if let Err(e) = self.broadcast_quorum_reached(block_hash).await {
                self.env_data.logger.log(format!(
                    "quorum reached locally for block {} but the status broadcast failed: {:?}",
                    bytes_to_hex(block_hash),
                    e
                ));
            }
        }
    }

    /// Replay votes that arrived before their block's stake set, then re-check
    /// quorum. Called from `handle_block_finalized` right after the stake set is
    /// cached, so "early" votes are not lost.
    pub(crate) async fn replay_pending_votes(&self, block_hash: &[u8]) {
        let buffered = {
            let mut pending = self.pending_confirmation_votes.lock().await;
            pending.remove(block_hash)
        };
        let Some(voters) = buffered else { return };

        let mut replayed = 0u64;
        for voter_key in voters {
            if self.accumulate_vote(block_hash, voter_key).await.is_some() {
                replayed += 1;
            }
        }
        if replayed > 0 {
            self.env_data.logger.log(format!(
                "replayed {} confirmation vote(s) for block {} that arrived before its stake set",
                replayed,
                bytes_to_hex(block_hash)
            ));
            self.announce_quorum_if_reached(block_hash).await;
        }
    }

    /// Handle "BlockQuorumReached" status update.
    ///
    /// A peer's claim is a **hint to re-check, not authorization**. This node
    /// re-runs its own stake-weighted comparison and flips the block to
    /// `Confirmed` only when its own accumulation satisfies quorum.
    ///
    /// It used to read "No further quorum check needed — the broadcaster verified
    /// quorum", which was the whole gate handed to the broadcaster's say-so, and
    /// the role gate let *any registered node of any role* be that broadcaster
    /// (`allowed_senders_for` now requires `Committer`). Since ADR-005/ADR-010
    /// make committer-side stake quorum the only quorum an ordinary transaction
    /// ever passes, accepting the claim unverified meant one node could mark any
    /// block final for everyone.
    pub(crate) async fn handle_block_quorum_reached(&self, message: Message) -> Result<(), CommitterError> {
        let block_hash: Vec<u8> =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        match self.local_quorum_reached(&block_hash).await {
            Some(true) => {}
            Some(false) => {
                self.rejected_quorum_claims.fetch_add(1, Ordering::Relaxed);
                self.env_data.logger.log(format!(
                    "SECURITY: BlockQuorumReached for block {} from {} does not match local stake \
                     arithmetic; finality NOT upgraded",
                    bytes_to_hex(&block_hash),
                    bytes_to_hex(&message.public_key)
                ));
                return Ok(());
            }
            None => {
                // Cannot judge yet: no stake set or no votes recorded locally. The
                // claim may well be true and our inputs simply early — but acting
                // on it would be trusting the broadcaster again.
                self.rejected_quorum_claims.fetch_add(1, Ordering::Relaxed);
                self.env_data.logger.log(format!(
                    "BlockQuorumReached for block {} cannot be verified locally (no stake set or \
                     no votes recorded); finality NOT upgraded",
                    bytes_to_hex(&block_hash)
                ));
                return Ok(());
            }
        }

        // Find matching token id without holding a read lock across mutation.
        let mut matched_token_id: Option<Vec<u8>> = None;
        for token_entry in self.tokens.iter() {
            let token = token_entry.value();
            let count = token.blockchain.get_count();
            if count == 0 {
                continue;
            }
            if let Some(tip) = token.blockchain.get_block_at(count - 1) {
                if tip.current_hash == block_hash {
                    matched_token_id = Some(token.id.clone());
                    break;
                }
            }
        }

        if let Some(token_id) = matched_token_id {
            let mut entry = self.tokens.get_mut(&token_id).ok_or_else(|| {
                CommitterError::TokenNotFound(bytes_to_hex(&token_id))
            })?;
            let _ = entry.value_mut().blockchain.set_finality_status(&block_hash, FinalityStatus::Confirmed);
        }

        Ok(())
    }

    /// Count of quorum claims this node refused to act on. Test/measurement
    /// accessor (Phase 0 item 3); non-zero with a healthy cluster means peers
    /// disagree about a block's votes, which is worth investigating.
    pub fn rejected_quorum_claim_count(&self) -> u64 {
        self.rejected_quorum_claims.load(Ordering::Relaxed)
    }

    /// Count of votes that arrived before their block's stake set and were
    /// buffered rather than dropped. Test/measurement accessor.
    pub fn buffered_confirmation_vote_count(&self) -> u64 {
        self.buffered_confirmation_votes.load(Ordering::Relaxed)
    }

    /// Broadcast BlockQuorumReached to all peer node types.
    pub(crate) async fn broadcast_quorum_reached(&self, block_hash: &[u8]) -> Result<(), CommitterError> {
        let body = serialize_to_bytes_rmp(&block_hash.to_vec())
            .map_err(|_| CommitterError::InternalSerialization)?;

        let message = Message::signed(
            self.env_data.environment_id.clone(),
            "BlockQuorumReached",
            body,
            None,
            &self.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&message)
            .map_err(|_| CommitterError::InternalSerialization)?;

        // Broadcast to all other node types
        let _ = self.node_registry.send_to_all(payload.clone(), &NodeRegistryType::Committer).await;
        let _ = self.node_registry.send_to_all(payload.clone(), &NodeRegistryType::Archiver).await;
        let _ = self.node_registry.send_to_all(payload.clone(), &NodeRegistryType::Executor).await;
        let _ = self.node_registry.send_to_all(payload, &NodeRegistryType::Sentinel).await;

        Ok(())
    }

    /// Broadcast a BlockConfirmed vote from this committer.
    /// Called after handle_block_finalized to vote on a block.
    pub(crate) async fn broadcast_vote(&self, block_hash: &[u8]) {
        // Serialize vote: (block_hash, this node's public key)
        let body = serialize_to_bytes_rmp(&(block_hash.to_vec(), self.public_key.clone()))
            .map_err(|_| ());

        let message = Message::signed(
            self.env_data.environment_id.clone(),
            "BlockConfirmed",
            body.unwrap_or_default(),
            None,
            &self.identity,
        )
        .map_err(|_| ());

        let payload = message
            .and_then(|m| serialize_to_bytes_rmp(&m).map_err(|_| ()));

        if let Ok(p) = payload {
            let _ = self.node_registry.send_to_all(p.clone(), &NodeRegistryType::Committer).await;
            let _ = self.node_registry.send_to_all(p.clone(), &NodeRegistryType::Archiver).await;
            let _ = self.node_registry.send_to_all(p.clone(), &NodeRegistryType::Executor).await;
            let _ = self.node_registry.send_to_all(p, &NodeRegistryType::Sentinel).await;
        }
    }
}
