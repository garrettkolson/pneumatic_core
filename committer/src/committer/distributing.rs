//! Production commit-path logic for the Committer, extracted from
//! `crate::committer` to keep that module focused on routing and construction.
//!
//! These are `impl Committer` methods, so they retain access to the struct's
//! private fields (child modules of `crate::committer`).

use super::*;

impl Committer {

    /// Handle token distribution from other committers.
    ///
    /// Inserts a freshly-distributed token into the local cache for future commits. A token IS its
    /// own blockchain — its chain and metadata are authoritative, so a peer may not swap in an
    /// alternative under an id that already exists (AUDIT Phase 5.5 / H13). A distribution carrying
    /// a new id is accepted (this is how a node joining the network seeds a token it lacks); a
    /// distribution whose id is already cached is refused and the cached token is left intact.
    pub(crate) async fn handle_token_distribution(&self, message: Message) -> Result<(), CommitterError> {
        let token: Token = deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        // Reject-on-conflict: `entry()` atomically checks-and-inserts under a single shard write
        // guard, the same single-operation shape as `handle_block_finalized`'s `get_mut`
        // (AUDIT Phase 3.3 / C5). A `contains_key`-then-`insert` would leave a read-then-write gap
        // where two concurrent distributions for a not-yet-existing id could both pass the check and
        // one would silently overwrite the other — the same swap vector we are closing here.
        match self.tokens.entry(token.id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(token);
                Ok(())
            }
            Entry::Occupied(_) => Err(self.token_distribution_conflict_err(&token.id)),
        }
    }


    /// Handle block distribution from other committers.
    /// Logs receipt for observability.
    pub(crate) async fn handle_block_distribution(&self, message: Message) -> Result<(), CommitterError> {
        let block: pneumatic_core::blocks::Block =
            deserialize_rmp_to(&message.body).map_err(CommitterError::Deserialization)?;

        let logger = &self.env_data.logger;
        logger.log(format!(
            "Received distributed block (hash: {})",
            bytes_to_hex(&block.current_hash)
        ));

        Ok(())
    }
}
