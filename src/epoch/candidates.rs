//! `CandidateRegistry`: bounded (LRU) competing-block candidates per
//! (token, previous_hash) position, with conflict detection on insert.

use super::*;

// ---------------------------------------------------------------------------
// CandidateRegistry — competing block proposals
// ---------------------------------------------------------------------------

/// Registry of competing block candidates keyed by (token_id, previous_hash).
/// Used for conflict detection — when two or more valid blocks reference the
/// same previous_hash for the same token, they represent a fork.
///
/// Each (token_id, previous_hash) group is bounded to at most
/// [`CandidateRegistry::DEFAULT_MAX_CANDIDATES`] candidates: when an insert
/// would exceed that cap the oldest candidate is evicted (LRU), so repeated
/// conflicting proposals at one position cannot inflate the registry without
/// bound (AUDIT Phase 5.2 / H2).
#[derive(Debug)]
pub struct CandidateRegistry {
    /// (token_id, previous_hash) → list of candidate (block, proposer_key) pairs
    candidates: DashMap<(Vec<u8>, Vec<u8>), Vec<(crate::blocks::Block, Vec<u8>)>>,
    /// Max number of candidates held at a single (token_id, previous_hash)
    /// position before the oldest is evicted.
    max_candidates: usize,
}

impl CandidateRegistry {
    /// Upper bound on the number of competing candidates kept at any one
    /// (token_id, previous_hash) position. Older candidates are evicted (LRU)
    /// once this many are present (AUDIT Phase 5.2 / H2).
    pub const DEFAULT_MAX_CANDIDATES: usize = 1024;

    pub fn new() -> Self {
        CandidateRegistry {
            candidates: DashMap::new(),
            max_candidates: CandidateRegistry::DEFAULT_MAX_CANDIDATES,
        }
    }

    /// Build a registry with an explicit per-position cap.
    pub fn with_max_candidates(max_candidates: usize) -> Self {
        CandidateRegistry {
            candidates: DashMap::new(),
            max_candidates: max_candidates.max(1),
        }
    }

    /// Insert a candidate block. If another candidate already exists at this
    /// (token_id, previous_hash), the new candidate is appended — a conflict
    /// is detected when the vec has length >= 2.
    ///
    /// On overflow the oldest candidate is evicted so the per-position vec stays
    /// bounded (AUDIT Phase 5.2 / H2 / LRU eviction).
    pub fn insert(&self, token_id: Vec<u8>, previous_hash: Vec<u8>,
                  block: crate::blocks::Block, proposer_key: Vec<u8>) {
        let key = (token_id, previous_hash);
        let mut entry = self.candidates.entry(key).or_insert_with(Vec::new);
        entry.push((block, proposer_key));
        // LRU eviction: keep at most `max_candidates` candidates per position.
        while entry.len() > self.max_candidates {
            entry.remove(0);
        }
    }

    /// Get all candidates for a given (token_id, previous_hash).
    pub fn get_candidates(&self, token_id: &[u8], previous_hash: &[u8]) -> Vec<(crate::blocks::Block, Vec<u8>)> {
        let key = (token_id.to_vec(), previous_hash.to_vec());
        self.candidates.get(&key)
            .map(|entry| entry.value().clone())
            .unwrap_or_default()
    }

    /// Check if a conflict exists: 2+ candidates at the same (token_id, previous_hash).
    pub fn has_conflict(&self, token_id: &[u8], previous_hash: &[u8]) -> bool {
        let key = (token_id.to_vec(), previous_hash.to_vec());
        self.candidates.get(&key)
            .map(|entry| entry.value().len() >= 2)
            .unwrap_or(false)
    }

    /// Get the number of candidates at a specific key.
    pub fn candidate_count(&self, token_id: &[u8], previous_hash: &[u8]) -> usize {
        let key = (token_id.to_vec(), previous_hash.to_vec());
        self.candidates.get(&key)
            .map(|entry| entry.value().len())
            .unwrap_or(0)
    }

    /// Remove all candidates at a key (after resolving a conflict).
    pub fn remove_conflicted(&self, token_id: &[u8], previous_hash: &[u8]) -> usize {
        let key = (token_id.to_vec(), previous_hash.to_vec());
        self.candidates.remove(&key).map(|(_, v)| v.len()).unwrap_or(0)
    }

    /// Total number of distinct (token_id, previous_hash) keys.
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// Returns true if there are no candidate groups.
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }
}

impl Default for CandidateRegistry {
    /// An empty registry sized to [`CandidateRegistry::DEFAULT_MAX_CANDIDATES`].
    fn default() -> Self {
        Self::new()
    }
}
