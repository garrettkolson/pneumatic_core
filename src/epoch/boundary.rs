//! `EpochBoundaryDetector`: wall-clock epoch advancement, current/previous
//! leader tracking, and stale-block detection against the previous leader.

use super::*;

// ---------------------------------------------------------------------------
// EpochBoundaryDetector — stale block and epoch advancement detection
// ---------------------------------------------------------------------------

/// Detects epoch expiry, stale blocks, and advances to new epochs.
#[derive(Debug, Clone)]
pub struct EpochBoundaryDetector {
    /// The current epoch
    pub current_epoch: Epoch,
    /// The leader from the previous epoch (for stale block detection)
    pub previous_leader: Option<Vec<u8>>,
}

impl EpochBoundaryDetector {
    pub fn new(epoch: Epoch) -> Self {
        EpochBoundaryDetector {
            current_epoch: epoch,
            previous_leader: None,
        }
    }

    /// Check if the current epoch has expired at the given timestamp.
    pub fn is_epoch_expired(&self, now: i64) -> bool {
        now >= self.current_epoch.end_timestamp
    }

    /// Return the current epoch's leader.
    pub fn current_leader(&self) -> Option<&[u8]> {
        if self.current_epoch.leader_public_key.is_empty() {
            None
        } else {
            Some(&self.current_epoch.leader_public_key)
        }
    }

    /// Advance to a new epoch: bump the epoch number, select a new leader.
    ///
    /// `prev_block_hash` is the chain tip of the epoch about to end; it is bound
    /// into the leader seed so the new leader is only knowable once that tip is
    /// mined (Phase 5.3 / AUDIT H3). Empty at genesis.
    pub fn advance_to_new_epoch(
        &mut self,
        selector: &dyn IEpochLeaderSelector,
        stake_set: &StakeSet,
        epoch_duration: i64,
        prev_block_hash: &[u8],
    ) {
        // Save current leader as previous
        if !self.current_epoch.leader_public_key.is_empty() {
            self.previous_leader = Some(self.current_epoch.leader_public_key.clone());
        }
        // Create new epoch
        let now = self.current_epoch.end_timestamp;
        let new_epoch_number = self.current_epoch.epoch_number + 1;
        self.current_epoch = Epoch::new_with_leader(
            new_epoch_number,
            now,
            now + epoch_duration,
            selector,
            stake_set,
            prev_block_hash,
        );
    }

    /// Check if a block was proposed by a stale (previous-epoch) leader.
    pub fn is_stale_block(&self, proposer_key: &[u8]) -> bool {
        match &self.previous_leader {
            Some(prev) => prev == proposer_key,
            None => false,
        }
    }
}
