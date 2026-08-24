//! Bounded, per-token orphan buffer for finalized blocks received out of order
//! (AUDIT Phase 3.4 / finding H15).
//!
//! When a `BlockFinalized` gossip event arrives whose block does not chain onto the current chain
//! tip, the block cannot be appended yet — its parent block has not landed. Rather than silently
//! discarding it (which permanently loses the block and everything chained off it on
//! out-of-order delivery), the Committer buffers it here. As the tip advances, [`crate::committer::Committer`]
//! re-evaluates the buffer and promotes any buffered block whose parent has just been appended.
//!
//! The buffer is intentionally small and self-cleaning:
//! * a hard global entry cap (`max_entries`) — once reached, [`OrphanBuffer::insert`] rejects
//!   further blocks (returned to the caller as `RejectedFull` to log) rather than silently evicting
//!   an existing one;
//! * bounded per token (`max_per_token`) — overflow for a single token evicts that token's oldest
//!   block;
//! * TTL'd (`ttl`) — a block whose parent never arrives is dropped on the next re-evaluation
//!   instead of lingering forever.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use pneumatic_core::blocks::Block;

/// A finalized block received out of order, held until its parent block lands.
#[derive(Debug, Clone)]
struct BufferedBlock {
    /// The block itself.
    block: Block,
    /// Hash of the block this one chains onto (`block.previous_hash`). The chain tip must reach
    /// this value before the block can be appended.
    parent_hash: Vec<u8>,
    /// When the block was buffered. Used for TTL expiry so a block whose parent never arrives is
    /// dropped rather than held forever.
    inserted_at: Instant,
    /// TTL this entry is held under. Kept per-entry so [`BufferedBlock::is_expired`] is a pure
    /// function of `inserted_at` + `ttl`, unit-testable without faking wall-clock time.
    ttl: Duration,
}

impl BufferedBlock {
    /// Whether the block has been buffered for at least its `ttl` at `now`.
    fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.inserted_at) >= self.ttl
    }
}

/// Result of trying to buffer an out-of-order block (AUDIT Phase 3.4 / H15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BufferDecision {
    /// The block was buffered and will be replayed once the tip advances.
    Buffered,
    /// The buffer is at capacity — the block was dropped rather than silently ignored. The caller
    /// must log this so the drop is observable, never a silent no-op.
    RejectedFull,
}

/// Bounded, per-token orphan buffer for finalized blocks whose `previous_hash` is not the current
/// chain tip. Prevents the silent drop of out-of-order `BlockFinalized` gossip (AUDIT Phase 3.4 /
/// H15).
#[derive(Debug)]
pub struct OrphanBuffer {
    /// token_id -> buffered blocks, oldest first (FIFO) so eviction drops the longest-held block.
    by_token: HashMap<Vec<u8>, VecDeque<BufferedBlock>>,
    /// Hard global cap on buffered blocks across all tokens.
    max_entries: usize,
    /// Per-token cap; overflow evicts the oldest block for that single token.
    max_per_token: usize,
    /// TTL: buffered blocks older than this are dropped on the next re-evaluation.
    ttl: Duration,
    /// Total buffered blocks across all tokens (fast capacity check without iterating).
    count: usize,
}

impl OrphanBuffer {
    /// Create an empty buffer with the given bounds.
    pub fn new(max_entries: usize, max_per_token: usize, ttl: Duration) -> Self {
        Self {
            by_token: HashMap::new(),
            max_entries,
            max_per_token,
            ttl,
            count: 0,
        }
    }

    /// Total buffered blocks across all tokens.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether nothing is currently buffered.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Blocks buffered for `token_id`, oldest first (oldest at index 0). Tests-only — the return
    /// type exposes the private [`BufferedBlock`], so it is gated behind `#[cfg(test)]`.
    #[cfg(test)]
    fn pending(&self, token_id: &[u8]) -> Option<&VecDeque<BufferedBlock>> {
        self.by_token.get(token_id)
    }

    /// Try to buffer `block` for `token_id`.
    ///
    /// Reclaims expired entries first. If the global cap is already full the block is rejected
    /// (never dropped silently) — the caller logs the drop. Otherwise the per-token cap for this
    /// token is evicted down before the block is buffered.
    pub(crate) fn insert(&mut self, token_id: Vec<u8>, block: Block) -> BufferDecision {
        // Reclaim expired entries so a full-looking buffer can be refilled.
        self.drop_expired(Instant::now());

        // The global cap is a hard, observable bound: reject (the caller logs) rather than
        // silently dropping some other buffered block to make room.
        if self.count >= self.max_entries {
            return BufferDecision::RejectedFull;
        }

        // Enforce the per-token cap for this token.
        let token_queue = self.by_token.entry(token_id).or_default();
        while token_queue.len() >= self.max_per_token {
            if token_queue.pop_front().is_some() {
                self.count -= 1;
            } else {
                break;
            }
        }

        let parent_hash = block.previous_hash.clone();
        token_queue.push_back(BufferedBlock {
            block,
            parent_hash,
            inserted_at: Instant::now(),
            ttl: self.ttl,
        });
        self.count += 1;
        BufferDecision::Buffered
    }

    /// Select and remove one buffered block for `token_id` whose parent hash matches `parent_hash`
    /// and that has not expired. Returns the block (`None` if no live block chains onto that
    /// parent). Expired entries are dropped first. Returns the block — not the wrapper — so the
    /// private [`BufferedBlock`] type never crosses the module boundary.
    pub fn take_matching(&mut self, token_id: &[u8], parent_hash: &[u8], now: Instant) -> Option<Block> {
        let queue = self.by_token.get_mut(token_id)?;
        let idx = queue
            .iter()
            .position(|b| !b.is_expired(now) && b.parent_hash.as_slice() == parent_hash)?;
        let removed = queue.remove(idx)?;
        self.count -= 1;
        if queue.is_empty() {
            self.by_token.remove(token_id);
        }
        Some(removed.block)
    }

    /// Re-place a previously-selected block into the buffer. Used when a block was removed for
    /// replay but could not be appended because a sibling advanced the tip in the meantime — the
    /// caller has already decremented `count` (in [`Self::take_matching`]), so this only restores
    /// the entry without recounting. The buffered `parent_hash` is derived from `block.previous_hash`
    /// (the requeued block chains onto the same parent).
    pub fn requeue_back(&mut self, token_id: &[u8], block: Block) {
        self.count += 1;
        let parent_hash = block.previous_hash.clone();
        self.by_token
            .entry(token_id.to_vec())
            .or_default()
            .push_back(BufferedBlock {
                block,
                parent_hash,
                inserted_at: Instant::now(),
                ttl: self.ttl,
            });
    }

    /// Drop every buffered block older than `now`.
    pub fn drop_expired(&mut self, now: Instant) {
        let mut removed_keys = Vec::new();
        for (key, queue) in self.by_token.iter_mut() {
            let before = queue.len();
            queue.retain(|b| !b.is_expired(now));
            let dropped = before - queue.len();
            if dropped > 0 {
                self.count -= dropped;
                if queue.is_empty() {
                    removed_keys.push((*key).clone());
                }
            }
        }
        for key in removed_keys {
            self.by_token.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pneumatic_core::blocks::{Block, BlockFactory, FinalityStatus};
    use pneumatic_core::transactions::{SignedTransaction, Transaction, TransactionSignature};
    use std::collections::HashMap;
    use std::time::Duration as StdDuration;

    /// Build a bare block (empty tx, no signatures) solely to exercise buffer mechanics.
    fn stub_block(previous_hash: &[u8]) -> Block {
        let signed = SignedTransaction {
            transaction_id: "stub".to_string(),
            transaction: Transaction {
                id: "stub".to_string(),
                action: "Process".into(),
                token_id: vec![1],
                bid: None,
                sequence_number: 1,
                sender: b"alice".to_vec(),
                receiver: b"bob".to_vec(),
                amount: Some(100),
                timestamp: 0,
                result_hash: vec![],
                sender_signature: vec![],
            },
            total_voters: 3,
            total_stake: 42,
            leader_hash: previous_hash.to_vec(),
            leader_address: vec![],
            leader_stake: 0,
            finalizer_addr: vec![],
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: b"stub".to_vec(),
                signature: vec![],
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: vec![],
        };
        let mut block = Block {
            signed_trans: signed,
            token_metadata: HashMap::new(),
            previous_hash: previous_hash.to_vec(),
            current_hash: vec![],
            timestamp: 0,
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        block.current_hash = BlockFactory::create_hash(&block);
        block
    }

    #[test]
    fn insert_buffers_and_reports_decision() {
        let mut buf = OrphanBuffer::new(1024, 256, StdDuration::from_secs(30));
        let block = stub_block(b"parent-1");
        assert_eq!(buf.insert(vec![1], block.clone()), BufferDecision::Buffered);
        assert_eq!(buf.len(), 1);
        let pending = buf.pending(&vec![1]).expect("one buffered block");
        assert_eq!(pending[0].parent_hash, b"parent-1");
    }

    #[test]
    fn reject_full_when_global_cap_exceeded() {
        // Global cap of 1: the first block fills it, and the second insert — regardless of token —
        // is rejected (RejectedFull) rather than evicting the first block. This is the discriminator:
        // a full buffer must reject-and-log, never silently drop an existing entry to admit one.
        let mut buf = OrphanBuffer::new(1, 256, StdDuration::from_secs(30));
        assert_eq!(buf.insert(vec![1], stub_block(b"a")), BufferDecision::Buffered);
        assert_eq!(buf.insert(vec![2], stub_block(b"b")), BufferDecision::RejectedFull);
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn per_token_cap_evicts_oldest_for_that_token() {
        let mut buf = OrphanBuffer::new(1024, 2, StdDuration::from_secs(30));
        assert_eq!(buf.insert(vec![1], stub_block(b"a")), BufferDecision::Buffered);
        assert_eq!(buf.insert(vec![1], stub_block(b"b")), BufferDecision::Buffered);
        // Third block exceeds the per-token cap; the oldest (parent "a") is evicted.
        assert_eq!(buf.insert(vec![1], stub_block(b"c")), BufferDecision::Buffered);
        assert_eq!(buf.len(), 2);
        let pending = buf.pending(&vec![1]).unwrap();
        assert_eq!(pending[0].parent_hash, b"b");
        assert_eq!(pending[1].parent_hash, b"c");
    }

    #[test]
    fn reject_full_when_global_cap_reached() {
        // Global cap 2, generous per-token cap: two tokens fill the buffer, and the third (distinct
        // token) is rejected rather than evicting any existing block. The two in-flight blocks stay
        // buffered so they can still be promoted once their parents land.
        let mut buf = OrphanBuffer::new(2, 256, StdDuration::from_secs(30));
        assert_eq!(buf.insert(vec![1], stub_block(b"a")), BufferDecision::Buffered);
        assert_eq!(buf.insert(vec![2], stub_block(b"b")), BufferDecision::Buffered);
        assert_eq!(buf.insert(vec![3], stub_block(b"c")), BufferDecision::RejectedFull);
        assert_eq!(buf.len(), 2);
        assert!(buf.pending(&vec![1]).is_some());
        assert!(buf.pending(&vec![2]).is_some());
        assert!(buf.pending(&vec![3]).is_none()); // token 3's block was rejected, not buffered
    }

    #[test]
    fn take_matching_only_matches_parent_and_is_not_expired() {
        let mut buf = OrphanBuffer::new(1024, 256, StdDuration::from_secs(30));
        buf.insert(vec![1], stub_block(b"parent-x"));
        buf.insert(vec![1], stub_block(b"parent-y"));

        // Matches the parent of the first insert; the returned block's `previous_hash` is that
        // parent (a buffered block's `parent_hash` is its `previous_hash`).
        let chosen = buf.take_matching(&vec![1], b"parent-x", Instant::now()).expect("matches");
        assert_eq!(chosen.previous_hash, b"parent-x");
        assert!(buf.pending(&vec![1]).is_some());

        // Now only the parent-y block remains.
        let chosen = buf.take_matching(&vec![1], b"parent-y", Instant::now()).expect("matches");
        assert_eq!(chosen.previous_hash, b"parent-y");
        assert!(buf.is_empty());

        // No match for an unrelated parent.
        assert!(buf.take_matching(&vec![1], b"nope", Instant::now()).is_none());
    }

    #[test]
    fn is_expired_respects_ttl() {
        // Inserted 1s ago with a 5s TTL: not yet expired.
        let ttl = StdDuration::from_secs(5);
        let block = stub_block(b"p");
        let inserted = Instant::now() - StdDuration::from_secs(1);
        let buffered = BufferedBlock {
            block,
            parent_hash: b"p".to_vec(),
            inserted_at: inserted,
            ttl,
        };
        assert!(!buffered.is_expired(Instant::now()));
        // Once the full TTL has elapsed past insertion it is expired (6s >= 5s).
        assert!(buffered.is_expired(inserted + StdDuration::from_secs(6)));
    }

    #[test]
    fn drop_expired_removes_old_entries() {
        let mut buf = OrphanBuffer::new(1024, 256, StdDuration::from_secs(5));
        buf.insert(vec![1], stub_block(b"a"));
        buf.insert(vec![1], stub_block(b"b"));
        assert_eq!(buf.len(), 2);

        // A block inserted "10s ago" is stale relative to now.
        let now = Instant::now();
        for q in buf.by_token.values_mut() {
            if let Some(front) = q.front_mut() {
                front.inserted_at = now - StdDuration::from_secs(10);
            }
        }
        buf.drop_expired(now);
        assert_eq!(buf.len(), 1); // the stale front block was dropped
        let remaining = buf.pending(&vec![1]).unwrap();
        assert_eq!(remaining[0].parent_hash, b"b");
    }
}
