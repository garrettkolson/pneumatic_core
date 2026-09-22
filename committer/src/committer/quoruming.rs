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

        // Look up stake set for this block
        let cumulative_stake = {
            let cache = self.stake_set_cache.lock().await;
            let stake_set = match cache.get(&block_hash) {
                Some(ss) => ss,
                None => {
                    // Vote received before BlockFinalized — ignore
                    return Ok(());
                }
            };

            let voting_stake = stake_set.get_stake(&voter_key);

            // Skip our own vote (we already voted via handle_block_finalized)
            if voter_key == self.public_key {
                return Ok(());
            }

            // Accumulate vote
            let mut votes = self.confirmation_votes.lock().await;
            let entry = votes.entry(block_hash.clone()).or_insert_with(|| {
                (HashSet::new(), 0u64)
            });

            // Count stake only once per key
            if !entry.0.insert(voter_key) {
                // Already counted
                return Ok(());
            }

            entry.1 = entry.1.saturating_add(voting_stake);
            entry.1
        };

        // Check if quorum reached
        if cumulative_stake > 0 {
            let total_stake = {
                let cache = self.stake_set_cache.lock().await;
                cache.get(&block_hash).map(|ss| ss.total_stake())
            };

            if let Some(total) = total_stake {
                // AUDIT Phase 6.9 (Item B, discriminator): exact-integer quorum. Casting
                // u64 stake to f64 truncates above 2^52, so the f64 threshold can be off by
                // one at the boundary — an adversarial stake could reach or miss quorum
                // wrongly. cumulative/total >= quorum/100  <=>  cumulative*100 >= total*quorum,
                // all in u128 (u64::MAX * 100 fits in u128). quorum_percentage is f32
                // validated to (0,100]; round() to nearest whole percent.
                // AUDIT Phase 6.9 (Item B, discriminator): exact-integer quorum. Casting
                // u64 stake to f64 truncates above 2^52, so the f64 threshold can be off by
                // one at the boundary — an adversarial stake could reach or miss quorum
                // wrongly. cumulative/total >= quorum/100  <=>  cumulative*100 >= total*quorum,
                // all in u128 (u64::MAX * 100 fits in u128). quorum_percentage is f32
                // validated to (0,100]; round() to nearest whole percent.
                let quorum_pct = self.env_data.quorum_percentage.round() as u128;
                let reached = (cumulative_stake as u128) * 100 >= (total as u128) * quorum_pct;
                if reached {
                    // Quorum reached — broadcast status to all peers
                    let _ = self.broadcast_quorum_reached(&block_hash).await;
                }
            }
        }

        Ok(())
    }

    /// Handle "BlockQuorumReached" status update.
    ///
    /// All nodes that receive this transition the block to Confirmed.
    /// No further quorum check needed — the broadcaster verified quorum.
    pub(crate) async fn handle_block_quorum_reached(&self, message: Message) -> Result<(), CommitterError> {
        let block_hash: Vec<u8> =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

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
