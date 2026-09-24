//! `StakeSet` + `ExecutorSet`: the weighted key→stake maps, their
//! order-independent canonical fingerprints, and the conversion between
//! them.

use super::*;

// ---------------------------------------------------------------------------
// StakeSet — current staking state
// ---------------------------------------------------------------------------

/// Maps public keys to stake amounts for leader selection and quorum checks.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StakeSet {
    /// Public key -> stake amount
    pub stakers: std::collections::HashMap<Vec<u8>, u64>,
}

impl StakeSet {
    pub fn total_stake(&self) -> u64 {
        // AUDIT Phase 6.9: saturating_add so an adversarial stake sum saturates to u64::MAX
        // instead of panicking on overflow — a panic in the selection path is a node DoS.
        self.stakers.values().fold(0u64, |acc, &v| acc.saturating_add(v))
    }

    /// Get the stake for a specific public key
    pub fn get_stake(&self, key: &[u8]) -> u64 {
        self.stakers.get(key).copied().unwrap_or(0)
    }

    /// Convert to an ExecutorSet — active stakers become the executor pool.
    /// Used at epoch boundary to persist the executor set for shard assignment.
    pub fn to_executor_set(&self) -> ExecutorSet {
        ExecutorSet {
            executors: self.stakers.clone(),
        }
    }

    /// Canonical (deterministic) byte encoding for integrity binding.
    ///
    /// A `HashMap` has no stable iteration order, so an otherwise-identical
    /// `StakeSet` would produce a different MsgPack digest on different nodes —
    /// defeating the corruption check. We route the data through a `BTreeMap`
    /// (sorted keys) before serializing, so `canonical_bytes` is stable across
    /// save and load (same ordering discipline as `ExecutorSet::shuffler`).
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, Error> {
        let sorted: BTreeMap<Vec<u8>, u64> = self
            .stakers
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        serialize_to_bytes_rmp(&sorted)
    }

    /// SHA-256 fingerprint of `canonical_bytes()` — the value stored alongside a
    /// persisted snapshot and re-verified on load (AUDIT Phase 5.4 / H9/M8).
    /// Empty-stake sets still yield a well-defined 32-byte digest.
    pub fn fingerprint(&self) -> [u8; 32] {
        sha256(&self.canonical_bytes().unwrap_or_default())
            .try_into()
            .unwrap_or([0u8; 32])
    }
}

// ---------------------------------------------------------------------------
// ExecutorSet — shard-aware executor pool
// ---------------------------------------------------------------------------

/// Maps executor public keys to their stakes. Used for deterministic shard
/// assignment: the sentinel computes `f(tx_id, epoch, shard_count) → shard`
/// then routes the transaction only to executors in that shard.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ExecutorSet {
    /// Executor public key → stake amount
    pub executors: HashMap<Vec<u8>, u64>,
}

impl ExecutorSet {
    pub fn total_stake(&self) -> u64 {
        // AUDIT Phase 6.9: saturating_add (overflow panics in debug) so the sum degrades to
        // u64::MAX rather than panicking — shard/finalizer selection never crashes on stake.
        self.executors.values().fold(0u64, |acc, &v| acc.saturating_add(v))
    }

    pub fn len(&self) -> usize {
        self.executors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.executors.is_empty()
    }

    pub fn get_stake(&self, key: &[u8]) -> u64 {
        self.executors.get(key).copied().unwrap_or(0)
    }

    /// Convert to a StakeSet — for leader/finalizer selection.
    pub fn to_stake_set(&self) -> StakeSet {
        StakeSet {
            stakers: self.executors.clone(),
        }
    }

    /// Create a deterministic shuffler for this executor set at a given epoch.
    /// The shuffle is used for per-epoch shard reassignment (rotation).
    pub fn shuffler(&self, epoch_number: u64, prev_block_hash: &[u8]) -> Shuffler {
        let mut keys: Vec<Vec<u8>> = self.executors.keys().cloned().collect();
        // C6: sort before Fisher-Yates so the shuffle is independent of HashMap insertion
        // order (matches deterministic_select at epoch.rs:236-237). Without this the
        // shuffle's starting order — and therefore the shard partition — varies per node.
        keys.sort();
        Shuffler::new(keys, epoch_number, prev_block_hash)
    }

    /// Canonical (deterministic) byte encoding for integrity binding.
    /// See `StakeSet::canonical_bytes` — routes through a `BTreeMap` so the
    /// digest is stable across save and load.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, Error> {
        let sorted: BTreeMap<Vec<u8>, u64> = self
            .executors
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        serialize_to_bytes_rmp(&sorted)
    }

    /// SHA-256 fingerprint of `canonical_bytes()` — stored alongside a persisted
    /// executor set and re-verified on load (AUDIT Phase 5.4 / H9/M8).
    pub fn fingerprint(&self) -> [u8; 32] {
        sha256(&self.canonical_bytes().unwrap_or_default())
            .try_into()
            .unwrap_or([0u8; 32])
    }
}
