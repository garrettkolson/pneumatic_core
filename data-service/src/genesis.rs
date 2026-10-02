//! Genesis: the records a cluster needs *before* its first node can boot.
//!
//! # Why genesis has to exist at all
//!
//! Both node binaries fail closed at boot when the data service cannot answer:
//! the committer at `get_stake_snapshot` and the composite at the shielded-pool
//! load ("refusing to re-seed — re-seeding would forget prior spends"). An
//! empty store is therefore not a bootable cluster; it is an unreachable one.
//! This module writes the minimum set of records that turns an empty store into
//! a genesis one.
//!
//! # The records, and who reads each
//!
//! | Record | Read by | Where |
//! |---|---|---|
//! | `User { stake, fuel_balance, nonce }` per node | role selection (`RoleSelector` vs the per-type floors) | `bin/node-server.rs` `DataStakeProvider` |
//! | `StakeSet` snapshot per epoch | the registration stake gate and leader election | `StakeIndex` (epoch 1 at boot), finalizer quorum (epoch 0 on the standard path) |
//! | `ShieldedPoolState` | composite/committer boot load, fail-closed | `ShieldedPool::load` |
//! | a token keyed by the environment id | chain-tip resolution for routing | `DataProvider::latest_block_hash` → `get_token(env_id, env_id)`, called by the sentinel |
//!
//! Both stake records must exist: seeding only the user rows installs roles but
//! rejects registrations (the gate consults the snapshot and a cache miss is
//! stake 0 ⇒ reject), and seeding only the snapshot does the reverse.
//!
//! # Envelopes are built by the client, never here
//!
//! `apply` writes through a [`DataProvider`] — in production a real
//! `DefaultDataProvider` pointed back at the service — so the SHA-256 envelope
//! fingerprints are computed by `save_stake_snapshot` / `save_shielded_pool` in
//! `pneumatic_core`, the same code path a running node uses. A seeder that
//! hand-built envelopes would be a second implementation of a consensus-critical
//! fingerprint.

use std::fmt;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use pneumatic_core::data::{DataProvider, DataError, ShieldedPoolState};
use pneumatic_core::epoch::StakeSet;
use pneumatic_core::shielded::{root_to_bytes, IncrementalMerkleTree, DEFAULT_DEPTH};
use pneumatic_core::tokens::Token;
use pneumatic_core::user::User;

/// A funded, role-eligible validator identity in the genesis set.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GenesisNode {
    /// The node's **Ed25519** public key, hex-encoded. This is the key the node
    /// signs with and that the stake gates look up — not its RNS public key.
    pub public_key_hex: String,
    /// Stake granted. Must clear `Config::default_min_stake()` (10) to register,
    /// and the per-type floors in the env spec to install a role.
    pub stake: u64,
    /// Fuel/gas balance for the node's own transactions. Defaults to 0.
    #[serde(default)]
    pub fuel_balance: u64,
}

/// A funded account that is not a validator (a load-driver's test wallets).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GenesisAccount {
    pub public_key_hex: String,
    pub fuel_balance: u64,
    #[serde(default)]
    pub stake: u64,
}

/// The genesis description for one environment.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GenesisSpec {
    /// Must equal the env spec's `environment_id`. Also the partition the
    /// sentinel's chain-tip lookup reads, so `seed_partition_token` keys a
    /// token here.
    pub environment_id: String,
    /// The token partition (`EnvironmentMetadata::token_partition_id`) that
    /// holds stake snapshots and shielded-pool state.
    pub token_partition_id: String,
    /// Epochs to write stake snapshots for. Production reads epoch 1 at boot;
    /// the standard pipeline path also reads epoch 0, so the default is
    /// `[0, 1]`.
    #[serde(default = "default_snapshot_epochs")]
    pub stake_snapshot_epochs: Vec<u64>,
    /// Shielded root recency window carried into the genesis pool. Must match
    /// the env spec's `shielded_root_recency` (default 10) or a booting node
    /// rebuilds the pool with a different window.
    #[serde(default = "default_recency")]
    pub shielded_root_recency: usize,
    /// The validator set.
    #[serde(default)]
    pub nodes: Vec<GenesisNode>,
    /// Non-validator accounts to fund.
    #[serde(default)]
    pub accounts: Vec<GenesisAccount>,
    /// Seed the pristine shielded pool. Required for a composite/committer to
    /// boot against a real `DefaultDataProvider`, which never reports "absent".
    #[serde(default = "default_true")]
    pub seed_shielded_pool: bool,
    /// Seed an empty-chain token under the environment id so
    /// `latest_block_hash` resolves instead of erroring.
    #[serde(default = "default_true")]
    pub seed_partition_token: bool,
}

fn default_snapshot_epochs() -> Vec<u64> {
    vec![0, 1]
}

fn default_recency() -> usize {
    10
}

fn default_true() -> bool {
    true
}

/// What [`apply`] wrote — printed by the binary so a testnet launch is auditable.
#[derive(Debug, Default, Clone)]
pub struct GenesisReport {
    pub users_written: usize,
    pub stake_snapshots_written: usize,
    pub total_stake: u64,
    pub pool_seeded: bool,
    pub partition_token_seeded: bool,
}

impl fmt::Display for GenesisReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} users, {} stake snapshot(s), total stake {}, pool seeded: {}, partition token: {}",
            self.users_written,
            self.stake_snapshots_written,
            self.total_stake,
            self.pool_seeded,
            self.partition_token_seeded,
        )
    }
}

/// Why genesis could not be written.
#[derive(Debug)]
pub enum GenesisError {
    /// Reading the spec file failed.
    Io(std::io::Error),
    /// The spec did not parse.
    Json(serde_json::Error),
    /// A hex public key was malformed (caught here rather than at the first
    /// registration attempt, minutes later, on a different node).
    BadPublicKey { field: String, error: hex::FromHexError },
    /// The data service refused a write.
    Store(String),
    /// The spec was structurally fine but unusable (no epochs to seed, etc.).
    Invalid(String),
}

impl fmt::Display for GenesisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GenesisError::Io(e) => write!(f, "read genesis spec: {e}"),
            GenesisError::Json(e) => write!(f, "parse genesis spec: {e}"),
            GenesisError::BadPublicKey { field, error } => {
                write!(f, "malformed hex public key in {field}: {error}")
            }
            GenesisError::Store(m) => write!(f, "data service refused a genesis write: {m}"),
            GenesisError::Invalid(m) => write!(f, "invalid genesis spec: {m}"),
        }
    }
}

impl std::error::Error for GenesisError {}

impl From<std::io::Error> for GenesisError {
    fn from(e: std::io::Error) -> Self {
        GenesisError::Io(e)
    }
}

impl From<serde_json::Error> for GenesisError {
    fn from(e: serde_json::Error) -> Self {
        GenesisError::Json(e)
    }
}

/// Pristine shielded-pool state: the empty-tree root, no leaves, no nullifiers,
/// no applied deltas.
///
/// The root is derived (not hard-coded) as `root_to_bytes(empty tree.root())`.
/// `IncrementalMerkleTree::root()` returns `Fp::zero()` at zero leaves, which is
/// exactly the genesis tip `MerkleRootState::new` seeds — so a node booting
/// against this state rebuilds to the identical root instead of failing the
/// pool's integrity check.
pub fn genesis_pool_state() -> ShieldedPoolState {
    let tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    ShieldedPoolState {
        root: root_to_bytes(&tree.root()),
        leaf_count: 0,
        leaves: Vec::new(),
        nullifiers: Vec::new(),
        applied: Vec::new(),
    }
}

/// Load a spec from a JSON file.
pub fn load_spec(path: impl AsRef<Path>) -> Result<GenesisSpec, GenesisError> {
    let raw = fs::read(path.as_ref())?;
    Ok(serde_json::from_slice(&raw)?)
}

fn decode_key(hex_key: &str, field: &str) -> Result<Vec<u8>, GenesisError> {
    hex::decode(hex_key.trim()).map_err(|error| GenesisError::BadPublicKey {
        field: field.to_string(),
        error,
    })
}

/// Write the genesis records through `provider`.
///
/// Idempotent in the useful direction: every write is an upsert keyed by the
/// same coordinates a node reads, so re-running genesis after a wipe is safe
/// and re-running it against a live store overwrites that store's stake/users
/// (which is why the binary prints the report and this function takes an
/// explicit spec rather than guessing).
pub fn apply(
    spec: &GenesisSpec,
    provider: &dyn DataProvider,
) -> Result<GenesisReport, GenesisError> {
    if spec.stake_snapshot_epochs.is_empty() {
        return Err(GenesisError::Invalid(
            "stake_snapshot_epochs is empty — a node reads epoch 1 at boot and would refuse to start".into(),
        ));
    }

    let mut report = GenesisReport::default();
    let mut stakers: Vec<(Vec<u8>, u64)> = Vec::with_capacity(spec.nodes.len());

    // --- users (role selection + gas) -------------------------------------
    for node in &spec.nodes {
        let key = decode_key(&node.public_key_hex, "nodes[].public_key_hex")?;
        let mut user = User::new(key.clone());
        user.stake = node.stake;
        user.fuel_balance = node.fuel_balance;
        provider
            .save_user(&key, user, &spec.token_partition_id)
            .map_err(|e| store_err("save_user(node)", &e))?;
        report.users_written += 1;
        report.total_stake += node.stake;
        stakers.push((key, node.stake));
    }

    for account in &spec.accounts {
        let key = decode_key(&account.public_key_hex, "accounts[].public_key_hex")?;
        let mut user = User::new(key.clone());
        user.fuel_balance = account.fuel_balance;
        user.stake = account.stake;
        provider
            .save_user(&key, user, &spec.token_partition_id)
            .map_err(|e| store_err("save_user(account)", &e))?;
        report.users_written += 1;
        // Accounts contribute stake to the snapshot only when granted some, so a
        // purely-funded test wallet does not silently gain voting weight.
        if account.stake > 0 {
            report.total_stake += account.stake;
            stakers.push((key, account.stake));
        }
    }

    // --- stake snapshots (registration gate + leader election) ------------
    // One StakeSet shared by every seeded epoch: the gate and the leader
    // selector must agree on the same validator set, and a snapshot that omits
    // a booting node means that node's registrations are rejected.
    for epoch in &spec.stake_snapshot_epochs {
        let mut set = StakeSet::default();
        for (key, stake) in &stakers {
            set.stakers.insert(key.clone(), *stake);
        }
        provider
            .save_stake_snapshot(*epoch, set, &spec.token_partition_id)
            .map_err(|e| store_err(&format!("save_stake_snapshot({epoch})"), &e))?;
        report.stake_snapshots_written += 1;
    }

    // --- shielded pool (composite boot is fail-closed on this read) -------
    if spec.seed_shielded_pool {
        let state = genesis_pool_state();
        provider
            .save_shielded_pool(&state, &spec.token_partition_id)
            .map_err(|e| store_err("save_shielded_pool", &e))?;
        report.pool_seeded = true;
    }

    // --- partition token (sentinel chain-tip lookup) ----------------------
    if spec.seed_partition_token {
        // `latest_block_hash(partition)` resolves as
        // `get_token(partition_id, partition_id)`; the sentinel passes the
        // *environment id*, so the record is keyed there. An empty chain yields
        // `last_hash_in == []`, the genesis convention — not an error.
        let key = spec.environment_id.as_bytes().to_vec();
        provider
            .save_token(&key, Token::new(), &spec.environment_id)
            .map_err(|e| store_err("save_token(partition)", &e))?;
        report.partition_token_seeded = true;
    }

    Ok(report)
}

fn store_err(op: &str, e: &DataError) -> GenesisError {
    GenesisError::Store(format!("{op}: {e:?}"))
}
