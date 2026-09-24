//! Leader + executor selection: domain-tagged seeds (`derive_selection_seed`),
//! the deterministic `Shuffler`, `deterministic_select(_shard)`, and the
//! weighted `LeaderSelector` behind `IEpochLeaderSelector`.

use super::*;

// ---------------------------------------------------------------------------
// Deterministic selection seed — domain-separated, prev-block-hash bound
// ---------------------------------------------------------------------------
// Phase 5.3 (AUDIT H3): every selection seed is derived from a per-type domain
// byte, the epoch number, and the previous block hash, so a choice made for one
// purpose (e.g. leader election) can never be replayed as another (e.g. shard
// index), and so selection is only knowable once the previous block is actually
// mined (not merely from the public epoch number + stake set).
//
// The byte layout is fixed so every selection type hashes the same shape:
//   SHA-256(domain ‖ epoch_number(big-endian) ‖ prev_block_hash ‖ extra)
pub const LEADER_DOMAIN: u8 = 0x01;
pub const SHARD_SHUFFLE_DOMAIN: u8 = 0x02;
pub const FINALIZER_DOMAIN: u8 = 0x03;
pub const SHARD_INDEX_DOMAIN: u8 = 0x04;

/// Derive the 32-byte seed for a deterministic selection.
///
/// `domain` distinguishes the selection type (see the `*_DOMAIN` constants).
/// `prev_block_hash` binds the choice to the mined chain tip — empty at genesis.
/// `extra` is per-transaction salt for load distribution on the finalizer and
/// shard-index paths (empty for leader/shuffle).
pub fn derive_selection_seed(
    domain: u8,
    epoch_number: u64,
    prev_block_hash: &[u8],
    extra: &[u8],
) -> [u8; 32] {
    let mut input = Vec::with_capacity(1 + 8 + prev_block_hash.len() + extra.len());
    input.push(domain);
    input.extend_from_slice(&epoch_number.to_be_bytes());
    input.extend_from_slice(prev_block_hash);
    input.extend_from_slice(extra);
    let digest = ring::digest::digest(&ring::digest::SHA256, &input);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

// ---------------------------------------------------------------------------
// Shuffler — deterministic Fisher-Yates shuffle
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Shuffler — deterministic Fisher-Yates shuffle
// ---------------------------------------------------------------------------

/// Deterministic permutation of executor keys derived from a seed.
/// Used for per-epoch shard reassignment (rotation): each epoch, executors
/// are reshuffled into new shards, preventing stable cartel formation.
pub struct Shuffler {
    /// Original items (not mutated).
    items: Vec<Vec<u8>>,
    /// The last computed permutation, stored so we can return a reference.
    last_permutation: Vec<Vec<u8>>,
}

impl Shuffler {
    /// Create a new shuffler from `items`, seeded per-epoch and per-chain-tip.
    ///
    /// The seed is `SHA-256(SHARD_SHUFFLE_DOMAIN ‖ epoch_number ‖ prev_block_hash)`,
    /// which guarantees per-epoch determinism while binding the shuffle to the
    /// mined tip so it is not predictable before the previous block lands. The
    /// shuffled result is computed at construction time and returned by `shuffle()`.
    pub fn new(items: Vec<Vec<u8>>, epoch_number: u64, prev_block_hash: &[u8]) -> Self {
        let seed = derive_selection_seed(
            SHARD_SHUFFLE_DOMAIN,
            epoch_number,
            prev_block_hash,
            &[],
        );
        let mut rng = StdRng::from_seed(seed);

        let n = items.len();
        if n == 0 {
            return Shuffler {
                items,
                last_permutation: vec![],
            };
        }

        // Fisher-Yates shuffle
        let mut indices: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            let j = rng.gen_range(0..=i);
            indices.swap(i, j);
        }

        let last_permutation: Vec<Vec<u8>> = indices.iter()
            .map(|&i| items[i].clone())
            .collect();

        Shuffler { items, last_permutation }
    }

    /// Return the shuffled executor keys for this epoch.
    /// For the same epoch_number, always returns the same order.
    pub fn shuffle(&self) -> &[Vec<u8>] {
        &self.last_permutation
    }
}

pub fn deterministic_select(
    stakers: &StakeSet,
    domain: u8,
    seed_bytes: &[u8],
    epoch_number: u64,
    prev_block_hash: &[u8],
) -> Option<Vec<u8>> {
    let total = stakers.total_stake();
    if total == 0 {
        return None;
    }

    // Domain-separated seed bound to the mined tip:
    // SHA-256(domain ‖ epoch_number ‖ prev_block_hash ‖ seed_bytes)
    let seed = derive_selection_seed(domain, epoch_number, prev_block_hash, seed_bytes);
    let mut rng = StdRng::from_seed(seed);
    let target: u64 = rng.gen_range(0..total);

    // Deterministic iteration: sort keys lexicographically, skipping any staker
    // with zero stake (AUDIT Phase 6.6). A zero-stake key contributes nothing to
    // the cumulative range, but was still reachable as the `target == 0` winner
    // (the first sorted key hits `cumulative(0) >= target(0)`) and could be
    // elected as leader/finalizer with no stake at risk. Dropping zero keys
    // guarantees a positive-stake key is returned whenever `total > 0`.
    let mut keys: Vec<&Vec<u8>> = stakers
        .stakers
        .iter()
        .filter(|kv| *kv.1 > 0)
        .map(|kv| kv.0)
        .collect();
    keys.sort();

    let first_key = keys.first().map(|k| (*k).clone()); // backup: first positive-stake key
    let mut cumulative = 0u64;
    for key in keys {
        let stake = *stakers.stakers.get(key).unwrap();
        // AUDIT Phase 6.9: saturating add — cumulative is compared `>= target` (target < total),
        // so a saturated cumulative still resolves the walk without panicking on overflow.
        cumulative = cumulative.checked_add(stake).unwrap_or(u64::MAX);
        if cumulative >= target {
            return Some(key.clone());
        }
    }
    // Fallback: return the first positive-stake key (only reached if total == 0, already handled)
    first_key
}

// ---------------------------------------------------------------------------
// Executor sharding — deterministic shard selection per transaction
// ---------------------------------------------------------------------------

/// Select which executors in a shard should handle a transaction.
///
/// 1. Shard-index seed = SHA-256(SHARD_INDEX_DOMAIN ‖ epoch ‖ prev_block_hash ‖ tx_id)
///    — same tx + epoch + tip → same seed (tx_id spreads load across the txs)
/// 2. Shuffle executors deterministically
///    (SHA-256(SHARD_SHUFFLE_DOMAIN ‖ epoch ‖ prev_block_hash))
/// 3. Stake-balanced round-robin partition into `shard_count` shards
/// 4. `shard_index = derived_seed mod shard_count`
/// 5. Return executor public keys in the selected shard
pub fn deterministic_select_shard(
    executors: &ExecutorSet,
    shard_count: u32,
    tx_id: &str,
    epoch_number: u64,
    prev_block_hash: &[u8],
) -> Option<Vec<Vec<u8>>> {
    if executors.is_empty() {
        return None;
    }
    if shard_count == 0 {
        return None;
    }
    if shard_count == 1 {
        // No sharding: return all *positive-stake* executors, sorted so the set is
        // identical regardless of the ExecutorSet's HashMap insertion order (C6).
        // AUDIT Phase 6.6: zero-stake executors are excluded so a slashed-to-zero
        // (or never-staked) node is never listed as a responsible executor.
        let mut keys: Vec<Vec<u8>> = executors
            .executors
            .iter()
            .filter(|kv| *kv.1 > 0)
            .map(|(key, _)| key.clone())
            .collect();
        keys.sort();
        return Some(keys);
    }

    // Domain-separated shard-index seed bound to the mined tip:
    // SHA-256(SHARD_INDEX_DOMAIN ‖ epoch_number ‖ prev_block_hash ‖ tx_id)
    let seed = derive_selection_seed(SHARD_INDEX_DOMAIN, epoch_number, prev_block_hash, tx_id.as_bytes());
    let mut shard_rng = StdRng::from_seed(seed);
    let shard_index: u32 = shard_rng.gen_range(0..shard_count);

    // Shuffle executors deterministically (bound to the mined tip)
    let shuffler = executors.shuffler(epoch_number, prev_block_hash);
    let shuffled = shuffler.shuffle();
    if shuffled.is_empty() {
        return None;
    }

    // AUDIT Phase 6.6: exclude zero-stake executors from the shard partition so a
    // slashed-to-zero (or never-staked) node is never assigned to a shard as a
    // responsible executor. If none remain, the shard selection yields nothing.
    let positive: Vec<&Vec<u8>> = shuffled
        .iter()
        .filter(|key| executors.get_stake(key) > 0)
        .collect();
    if positive.is_empty() {
        return None;
    }

    // Stake-balanced round-robin: assign each executor to the shard
    // with the lowest current total stake
    let mut shard_stakes: Vec<u64> = vec![0; shard_count as usize];
    let mut shard_executors: Vec<Vec<Vec<u8>>> = vec![vec![]; shard_count as usize];

    for key in positive {
        let stake = executors.get_stake(key);
        // Find shard with lowest current stake
        let target_shard = (0..shard_count as usize)
            .min_by_key(|&i| shard_stakes[i])
            .unwrap_or(0);
        // AUDIT Phase 6.9: saturating add (overflow panics in debug) — degrade to u64::MAX.
        shard_stakes[target_shard] = shard_stakes[target_shard].checked_add(stake).unwrap_or(u64::MAX);
        shard_executors[target_shard].push(key.clone());
    }

    // Return executors for the selected shard
    let idx = shard_index as usize;
    if shard_executors[idx].is_empty() {
        return None;
    }
    Some(shard_executors[idx].clone())
}

// ---------------------------------------------------------------------------
// LeaderSelector — stake-weighted random selection
// ---------------------------------------------------------------------------

/// Stake-weighted random leader selector.
/// Uses the cumulative stake range approach: pick a random point
/// in [0, total_stake) and walk the sorted stakers to find who
/// owns that point.
pub struct LeaderSelector;

impl LeaderSelector {
    pub fn new() -> Self {
        LeaderSelector
    }
}

impl Default for LeaderSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl IEpochLeaderSelector for LeaderSelector {
    fn select(&self, stakers: &StakeSet, epoch_number: u64, prev_block_hash: &[u8]) -> Vec<u8> {
        deterministic_select(stakers, LEADER_DOMAIN, &[], epoch_number, prev_block_hash)
            .unwrap_or_default()
    }
}
