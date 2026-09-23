//! Epoch handling for the Sentinel (advance_epoch, block-finalized epoch bumps).
//!
//! These are `impl Sentinel` methods, so they retain access to the struct's
//! private fields (child modules of `crate::sentinel`).

use super::*;

impl Sentinel {
    /// Advance the sentinel to a new epoch.
    ///
    /// Invalidates the executor set cache so the next transaction triggers
    /// a fresh load + shuffle. Updates the tracked epoch number.
    ///
    /// Call this when a new epoch is detected (e.g., from chain blocks).
    pub fn advance_epoch(&self, epoch_number: u64) {
        // Fail-closed: never rewind the epoch. A stale or replayed block must not
        // roll routing back to an older executor/stake snapshot.
        if epoch_number <= *self.current_epoch.lock() {
            return;
        }
        *self.current_epoch.lock() = epoch_number;
        self.executor_set_cache.invalidate_all();
        self.stake_snapshot_cache.invalidate_all();
    }

    /// Advance the sentinel's epoch from a `BlockFinalized` gossip message.
    ///
    /// Fail-closed (AUDIT H5): only a registered finalizer may move the epoch, and
    /// the advance is monotonic (an `advance_epoch` guard rejects stale/replayed
    /// blocks). The `epoch_number` is bound into the block hash (Phase 2.1), so a
    /// gossiper-authenticated `BlockFinalized` from a registered finalizer is a
    /// trustworthy epoch signal. This is an availability signal, not a chain-append:
    /// the sentinel does not validate linkage here.
    pub(crate) fn handle_block_finalized_for_epoch(&self, message: Message) -> Result<(), SentinelError> {
        let block: Block = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // Fail-closed role guard: only a registered finalizer may advance the epoch.
        // `message.public_key` is the gossiper-authenticated sender identity.
        let is_finalizer = self
            .node_registry
            .get_nodes(&NodeRegistryType::Finalizer)
            .map(|nodes| nodes.iter().any(|n| n.key() == &message.public_key))
            .unwrap_or(false);
        if !is_finalizer {
            return Err(SentinelError::Registry(format!(
                "BlockFinalized from non-finalizer {:?}",
                message.public_key
            )));
        }

        self.advance_epoch(block.epoch_number);
        Ok(())
    }

}
