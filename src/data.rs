use std::net::IpAddr::V4;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::Deref;
use moka::sync::Cache;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use serde::{Deserialize, Serialize};
use serde_json::error::Category::Data;
use crate::conns::{ConnError, ConnTarget, LocalTarget};
use crate::crypto::sha256;
use crate::conns::factories::{ConnFactory, IsConnFactory};
use crate::conns::uds::data_socket_path;
use crate::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use crate::epoch::{ExecutorSet, StakeSet};
use crate::tokens::Token;
use crate::user::User;

pub const DATA_TCP_PORT: u16 = 55555;
pub const DATA_UNIX_PATH: &str = "data";

pub trait DataProvider : Send + Sync {
    fn get_token(&self, key: &Vec<u8>, partition_id: &str) -> Result<Token, DataError> {
        DefaultDataProvider::new().get_token(key, partition_id)
    }

    fn save_token(&self, key: &Vec<u8>, token: Token, partition_id: &str)
                  -> Result<(), DataError> {
        DefaultDataProvider::new().save_token(key, token, partition_id)
    }

    fn get_data(&self, key: &Vec<u8>, partition_id: &str) -> Result<Vec<u8>, DataError> {
        DefaultDataProvider::new().get_data(key, partition_id)
    }

    fn save_data(&self, key: &Vec<u8>, data: Vec<u8>, partition_id: &str) -> Result<(), DataError> {
        DefaultDataProvider::new().save_data(key, data, partition_id)
    }

    fn get_user(&self, key: &Vec<u8>, partition_id: &str) -> Result<User, DataError> {
        DefaultDataProvider::new().get_user(key, partition_id)
    }

    fn save_user(&self, key: &Vec<u8>, user: User, partition_id: &str) -> Result<(), DataError> {
        DefaultDataProvider::new().save_user(key, user, partition_id)
    }

    /// Retrieve a stake snapshot for a given epoch and partition.
    fn get_stake_snapshot(&self, epoch: u64, partition_id: &str) -> Result<StakeSet, DataError>;

    /// Persist a stake snapshot for a given epoch and partition.
    fn save_stake_snapshot(&self, epoch: u64, snapshot: StakeSet, partition_id: &str) -> Result<(), DataError>;

    /// Retrieve an executor set for a given epoch and partition.
    fn get_executor_set(&self, epoch: u64, partition_id: &str) -> Result<ExecutorSet, DataError>;

    /// Persist an executor set for a given epoch and partition.
    fn save_executor_set(&self, epoch: u64, set: ExecutorSet, partition_id: &str) -> Result<(), DataError>;

    /// Current chain-tip hash (previous block hash) for a token partition, or
    /// `None` if unknown. Used as the `prev_block_hash` input to deterministic
    /// selection seeds (Phase 5.3 / AUDIT H3): when a provider cannot resolve a
    /// tip, the seed simply falls back to an empty `prev_block_hash`.
    ///
    /// Returns `Ok(None)` by default so providers that never track the tip need
    /// no change; providers that can resolve a tip override this.
    fn latest_block_hash(&self, partition_id: &str) -> Result<Option<Vec<u8>>, DataError> {
        Ok(None)
    }

    /// Retrieve the committed shielded-pool state for a partition (S5.3).
    ///
    /// `Ok(None)` means a *true absence* (the key was never written — the
    /// committer seeds pristine genesis and persists it). Any `Err`
    /// (corruption, `SnapshotCorrupt`, a hung/unauthenticated service) is a
    /// fail-closed boot condition for a shielded-enabled committer: a pool
    /// the store cannot return verbatim must never be silently re-seeded,
    /// because re-seeding forgets prior spends (a double-spend window).
    ///
    /// The default `Ok(None)` keeps role-specific stubs compiling; production
    /// (`DefaultDataProvider`) and in-memory test stubs override it.
    fn get_shielded_pool(&self, partition_id: &str) -> Result<Option<ShieldedPoolState>, DataError> {
        Ok(None)
    }

    /// Persist the committed shielded-pool state for a partition (S5.3).
    ///
    /// This is the durable record that closes the crash-and-restart
    /// un-spend window (roadmap 2.5): the committer persists a delta here
    /// *before* reporting its block committed. The default `Ok(())` keeps
    /// role-specific stubs compiling; a provider that does not actually
    /// persist silently voids that guarantee — shielded-enabled deployments
    /// must use a provider that overrides this.
    fn save_shielded_pool(&self, state: &ShieldedPoolState, partition_id: &str) -> Result<(), DataError> {
        Ok(())
    }
}

pub struct DefaultDataProvider {
    conn_factory: ConnFactory,
    /// The data-service endpoint this provider talks to.
    source: ConnTarget,
}

/// Absolute default data-service endpoint: a per-UID socket path on Unix, TCP
/// loopback otherwise. Previously used a relative, world-writable-path `"data"`
/// which could be hijacked by a pre-created symlink at that path.
fn default_source() -> ConnTarget {
    let local_target = match cfg!(unix) {
        true => {
            let path = data_socket_path(DATA_UNIX_PATH)
                .unwrap_or_else(|_| std::path::PathBuf::from(format!("/tmp/{}.sock", DATA_UNIX_PATH)));
            LocalTarget::Unix(path.to_string_lossy().into_owned())
        }
        false => LocalTarget::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, DATA_TCP_PORT)))
    };

    ConnTarget::Local(local_target)
}

/// Translate a data-channel failure into a `DataError`. A blocked read/write
/// (hung data service) surfaces as `Timeout` rather than a generic store error,
/// and a failed shared-secret check surfaces as `PeerUnauthenticated`.
fn conn_error_to_data_error(err: ConnError) -> DataError {
    match err {
        ConnError::Timeout(msg) => DataError::Timeout(msg),
        ConnError::Unauthenticated(msg) => DataError::PeerUnauthenticated(msg),
        other => DataError::FromStore(other.to_string()),
    }
}

impl DefaultDataProvider {
    pub fn new() -> Self {
        DefaultDataProvider {
            conn_factory: ConnFactory::new(),
            source: default_source(),
        }
    }

    /// Rebuild the backing connection factory with a shared secret so every
    /// data-channel frame is HMAC-authenticated. The timeouts / framing / cap
    /// hardening apply regardless of whether a secret is configured.
    pub fn with_secret(mut self, secret: Vec<u8>) -> Self {
        self.conn_factory = ConnFactory::new().with_secret(Some(secret));
        self
    }

    /// Override the data-service endpoint (used by tests to point at a custom
    /// socket / port).
    pub fn with_source(mut self, source: ConnTarget) -> Self {
        self.source = source;
        self
    }

    /// Override the blocking read/write bound applied to the backing factory.
    /// Used by tests to prove a hung data service degrades to a `Timeout`
    /// rather than blocking forever; production callers can set their own.
    pub fn with_timeout(mut self, rw_timeout: Duration) -> Self {
        self.conn_factory = self.conn_factory.with_timeout(rw_timeout);
        self
    }

    /// The data-service endpoint this provider talks to.
    pub fn get_source(&self) -> ConnTarget {
        self.source.clone()
    }

    fn serialize_request(&self, key: &Vec<u8>, op: DataOp, partition: &str)
                         -> Result<Vec<u8>, DataError> {
        let request = DataRequest::new(key, op, partition);
        return match serialize_to_bytes_rmp(&request) {
            Ok(d) => Ok(d),
            Err(err) => Err(DataError::SerializationError(err))
        }
    }

    fn get_data_internal<T>(&self, key: &Vec<u8>, op: DataOp, partition: &str)
                            -> Result<T, DataError>
        where T : Serialize + for<'a> Deserialize<'a>
    {
        if let DataOp::Save(_) = op { return Err(DataError::InvalidOperation(op)) }
        let source = self.get_source();
        if let Ok(sender) = self.conn_factory.get_sender(source) {
            let data = self.serialize_request(key, op, partition)?;
            let response = match sender.get_response(&data) {
                Ok(data) => data,
                Err(err) => return Err(conn_error_to_data_error(err))
            };

            return match deserialize_rmp_to::<T>(&response) {
                Ok(token) => Ok(token),
                Err(err) => Err(DataError::DeserializationError(err))
            }
        }

        Err(DataError::StoreNotFound)
    }

    fn save_data_internal<T>(&self, key: &Vec<u8>, op: DataOp, partition: &str)
                             -> Result<(), DataError>
        where T : Serialize + for<'a> Deserialize<'a>
    {
        if let DataOp::Get(_) = op { return Err(DataError::InvalidOperation(op)) }
        let source = self.get_source();
        if let Ok(sender) = self.conn_factory.get_sender(source) {
            let data = self.serialize_request(key, op, partition)?;
            return match sender.get_response(&data) {
                Ok(_) => Ok(()),
                Err(err) => Err(conn_error_to_data_error(err))
            };
        }

        Err(DataError::StoreNotFound)
    }
}

impl DataProvider for DefaultDataProvider {
    fn get_token(&self, key: &Vec<u8>, partition_id: &str) -> Result<Token, DataError> {
        self.get_data_internal::<Token>(key, DataOp::Get(GetOp::Token), partition_id)
    }

    fn save_token(&self, key: &Vec<u8>, token: Token, partition_id: &str)
                  -> Result<(), DataError> {
        self.save_data_internal::<Token>(key, DataOp::Save(SaveOp::Token(token)), partition_id)
    }

    fn get_data(&self, key: &Vec<u8>, partition_id: &str) -> Result<Vec<u8>, DataError> {
        self.get_data_internal::<Vec<u8>>(key, DataOp::Get(GetOp::Data), partition_id)
    }

    fn save_data(&self, key: &Vec<u8>, data: Vec<u8>, partition_id: &str) -> Result<(), DataError> {
        self.save_data_internal::<Vec<u8>>(key, DataOp::Save(SaveOp::Data(data)), partition_id)
    }

    // `get_user` / `save_user` are implemented HERE on purpose.
    //
    // They used to live as private *inherent* methods on this type, which meant
    // this trait impl silently inherited the trait's default — and that default
    // builds a brand-new `DefaultDataProvider::new()` with the DEFAULT local
    // source. Every production caller holds `Arc<dyn DataProvider>` (both node
    // binaries, `ActionRouter`, `StakeIndex`, `DataStakeProvider`), so trait
    // dispatch won and the freshly built provider discarded `with_source` *and*
    // `with_secret`. With `PNEUMATIC_DATA_ADDR` pointing at a remote data
    // service — the container topology the operator runbook documents — user
    // lookups silently went to the local UDS path instead, found nothing, and
    // resolved stake 0: role selection installed no roles and the registration
    // gate rejected every peer. An in-crate test could not catch it, because
    // `src/data.rs` tests can see the private inherent method; callers outside
    // the module cannot.
    fn get_user(&self, key: &Vec<u8>, partition_id: &str) -> Result<User, DataError> {
        self.get_data_internal::<User>(key, DataOp::Get(GetOp::User), partition_id)
    }

    fn save_user(&self, key: &Vec<u8>, user: User, partition_id: &str) -> Result<(), DataError> {
        self.save_data_internal::<User>(key, DataOp::Save(SaveOp::User(user)), partition_id)
    }

    fn get_stake_snapshot(&self, epoch: u64, partition_id: &str) -> Result<StakeSet, DataError> {
        // Phase 5.4 / H9/M8: read the SHA-256 envelope, verify the fingerprint,
        // then return the plain stake set. A mismatch (corruption / a value that
        // differs from what was persisted) surfaces as `SnapshotCorrupt` rather
        // than silently trusting deserialized bytes.
        let env: StakeSnapshotEnvelope = self
            .get_data_internal::<StakeSnapshotEnvelope>(
                &epoch.to_be_bytes().to_vec(),
                DataOp::Get(GetOp::StakeSnapshot(epoch)),
                partition_id,
            )?;
        env.verify()?;
        Ok(env.payload)
    }

    fn save_stake_snapshot(&self, epoch: u64, snapshot: StakeSet, partition_id: &str) -> Result<(), DataError> {
        let env = StakeSnapshotEnvelope::new(snapshot, epoch);
        self.save_data_internal::<StakeSnapshotEnvelope>(
            &epoch.to_be_bytes().to_vec(),
            DataOp::Save(SaveOp::StakeSnapshot(env)),
            partition_id,
        )
    }

    fn get_executor_set(&self, epoch: u64, partition_id: &str) -> Result<ExecutorSet, DataError> {
        let env: ExecutorSetEnvelope = self
            .get_data_internal::<ExecutorSetEnvelope>(
                &epoch.to_be_bytes().to_vec(),
                DataOp::Get(GetOp::ExecutorSet(epoch)),
                partition_id,
            )?;
        env.verify()?;
        Ok(env.payload)
    }

    fn save_executor_set(&self, epoch: u64, set: ExecutorSet, partition_id: &str) -> Result<(), DataError> {
        let env = ExecutorSetEnvelope::new(set, epoch);
        self.save_data_internal::<ExecutorSetEnvelope>(
            &epoch.to_be_bytes().to_vec(),
            DataOp::Save(SaveOp::ExecutorSet(env)),
            partition_id,
        )
    }

    fn latest_block_hash(&self, partition_id: &str) -> Result<Option<Vec<u8>>, DataError> {
        // The chain tip lives in the persisted token's blockchain; reuse the
        // existing token lookup so no new data-service action is required.
        let token_id = partition_id.as_bytes().to_vec();
        let token = self.get_token(&token_id, partition_id)?;
        Ok(Some(token.blockchain.get_current_chain_state().last_hash_in))
    }

    fn get_shielded_pool(&self, partition_id: &str) -> Result<Option<ShieldedPoolState>, DataError> {
        // S5.3: read the SHA-256 envelope, verify the fingerprint, then return
        // the plain pool state. NOTE the fail-closed asymmetry with the
        // in-memory stubs: this provider never fabricates `Ok(None)` — a
        // missing key at the service level surfaces as a data error
        // (deserialization failure / service reply), and the committer treats
        // any `Err` as corrupt at boot. The conservative direction: we would
        // rather refuse to start than silently re-seed a pool whose prior
        // spends we cannot prove are gone.
        let env: ShieldedPoolStateEnvelope = self.get_data_internal(
            &b"shielded_pool".to_vec(),
            DataOp::Get(GetOp::ShieldedPool),
            partition_id,
        )?;
        env.verify()?;
        Ok(Some(env.payload))
    }

    fn save_shielded_pool(&self, state: &ShieldedPoolState, partition_id: &str) -> Result<(), DataError> {
        let env = ShieldedPoolStateEnvelope::new(state.clone());
        self.save_data_internal::<ShieldedPoolStateEnvelope>(
            &b"shielded_pool".to_vec(),
            DataOp::Save(SaveOp::ShieldedPool(env)),
            partition_id,
        )
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum DataOp {
    Get(GetOp),
    Save(SaveOp)
}

impl std::fmt::Display for DataOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataOp::Get(op) => write!(f, "Get({})", op),
            DataOp::Save(op) => write!(f, "Save({})", op),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum GetOp {
    Token,
    Data,
    User,
    StakeSnapshot(u64),
    ExecutorSet(u64),
    /// S5.3: the committed shielded-pool state for a partition (additive —
    /// ground rule 4: an old data service seeing the unknown tag fails closed).
    ShieldedPool,
}

impl std::fmt::Display for GetOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GetOp::Token => write!(f, "Token"),
            GetOp::Data => write!(f, "Data"),
            GetOp::User => write!(f, "User"),
            GetOp::StakeSnapshot(epoch) => write!(f, "StakeSnapshot({})", epoch),
            GetOp::ExecutorSet(epoch) => write!(f, "ExecutorSet({})", epoch),
            GetOp::ShieldedPool => write!(f, "ShieldedPool"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum SaveOp {
    Token(Token),
    Data(Vec<u8>),
    User(User),
    // Phase 5.4 / H9/M8: the payload travels inside a SHA-256 envelope so the
    // store round-trips an integrity-attested value. On load, the provider
    // recomputes `payload.fingerprint()` and rejects a mismatch as corruption
    // instead of trusting arbitrary deserialized bytes.
    StakeSnapshot(StakeSnapshotEnvelope),
    ExecutorSet(ExecutorSetEnvelope),
    // S5.3: the shielded pool state, same envelope discipline (additive —
    // ground rule 4).
    ShieldedPool(ShieldedPoolStateEnvelope),
}

/// SHA-256 envelope around a persisted stake snapshot.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StakeSnapshotEnvelope {
    /// The frozen stake set for the epoch (sentinel deterministic routing).
    pub payload: StakeSet,
    /// `payload.fingerprint()` — the SHA-256 of `payload.canonical_bytes()`.
    pub hash: [u8; 32],
    /// Epoch the snapshot was persisted for (mirrors the storage key).
    pub epoch: u64,
}

/// SHA-256 envelope around a persisted executor set. See `StakeSnapshotEnvelope`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecutorSetEnvelope {
    /// The executor pool for the epoch (sentinel shard assignment).
    pub payload: ExecutorSet,
    /// `payload.fingerprint()` — the SHA-256 of `payload.canonical_bytes()`.
    pub hash: [u8; 32],
    /// Epoch the set was persisted for (mirrors the storage key).
    pub epoch: u64,
}

impl StakeSnapshotEnvelope {
    /// Wrap a stake set, computing its SHA-256 fingerprint.
    pub fn new(payload: StakeSet, epoch: u64) -> Self {
        StakeSnapshotEnvelope {
            hash: payload.fingerprint(),
            payload,
            epoch,
        }
    }

    /// Verify the stored fingerprint matches the payload; `SnapshotCorrupt`
    /// otherwise (AUDIT Phase 5.4 / H9/M8).
    pub fn verify(&self) -> Result<(), DataError> {
        if self.hash != self.payload.fingerprint() {
            return Err(DataError::SnapshotCorrupt(format!(
                "epoch={}: stored hash != payload.fingerprint()",
                self.epoch
            )));
        }
        Ok(())
    }
}

impl ExecutorSetEnvelope {
    /// Wrap an executor set, computing its SHA-256 fingerprint.
    pub fn new(payload: ExecutorSet, epoch: u64) -> Self {
        ExecutorSetEnvelope {
            hash: payload.fingerprint(),
            payload,
            epoch,
        }
    }

    /// Verify the stored fingerprint matches the payload; `SnapshotCorrupt`
    /// otherwise (AUDIT Phase 5.4 / H9/M8).
    pub fn verify(&self) -> Result<(), DataError> {
        if self.hash != self.payload.fingerprint() {
            return Err(DataError::SnapshotCorrupt(format!(
                "epoch={}: stored hash != payload.fingerprint()",
                self.epoch
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shielded pool state persistence (Phase S5.3)
// ---------------------------------------------------------------------------

/// One applied block's shielded-pool delta (S5.3): the leaves it appended,
/// the nullifiers it spent, and the pool root that resulted.
///
/// Persisted in commit order; this ordered sequence is the pool's complete
/// history — the tree is rebuilt by re-appending the concatenated leaves,
/// the root history by pushing each `post_root`, and rollback is the exact
/// removal of one entry.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AppliedPoolDelta {
    /// The block hash (`current_hash`) of the block that produced the delta.
    pub block_hash: Vec<u8>,
    /// Pool leaves appended by the block (canonical 32-byte Fp encoding).
    pub leaves: Vec<[u8; 32]>,
    /// Nullifiers spent by the block.
    pub nullifiers: Vec<[u8; 32]>,
    /// Pool root after applying this delta (canonical 32-byte Fp encoding).
    pub post_root: [u8; 32],
}

/// Full shielded-pool state for S5.3 persistence.
///
/// All fields are fixed-order (`Vec`/primitives) — no `HashMap` — so the
/// serde MsgPack form is already canonical; `canonical_bytes` serializes
/// directly.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ShieldedPoolState {
    /// Current pool root (canonical 32-byte Fp encoding).
    pub root: [u8; 32],
    /// Number of leaves in the tree (== `leaves.len()`, asserted on load).
    pub leaf_count: u64,
    /// The full ordered leaf sequence (canonical 32-byte Fp encoding each).
    pub leaves: Vec<[u8; 32]>,
    /// The spent-nullifier set (the union of the applied deltas' nullifiers).
    pub nullifiers: Vec<[u8; 32]>,
    /// The applied deltas in commit order.
    pub applied: Vec<AppliedPoolDelta>,
}

impl ShieldedPoolState {
    /// Canonical bytes (see the type doc: the serde form is already
    /// canonical — no `HashMap` fields).
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, DataError> {
        serialize_to_bytes_rmp(self).map_err(DataError::SerializationError)
    }

    /// SHA-256 fingerprint of `canonical_bytes()` — the value stored in the
    /// envelope and re-verified on load (Phase 5.4 pattern).
    pub fn fingerprint(&self) -> [u8; 32] {
        sha256(&self.canonical_bytes().unwrap_or_default())
            .try_into()
            .unwrap_or([0u8; 32])
    }
}

/// SHA-256 envelope around a persisted shielded-pool state (S5.3). Mirrors
/// `StakeSnapshotEnvelope` (Phase 5.4 / H9/M8): the payload travels with its
/// digest, and a load that sees a mismatch surfaces `SnapshotCorrupt`
/// instead of trusting deserialized bytes. No epoch field — the pool is a
/// single per-partition value, not per-epoch.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShieldedPoolStateEnvelope {
    /// The pool state.
    pub payload: ShieldedPoolState,
    /// `payload.fingerprint()` — the SHA-256 of `payload.canonical_bytes()`.
    pub hash: [u8; 32],
}

impl ShieldedPoolStateEnvelope {
    /// Wrap a pool state, computing its SHA-256 fingerprint.
    pub fn new(payload: ShieldedPoolState) -> Self {
        ShieldedPoolStateEnvelope {
            hash: payload.fingerprint(),
            payload,
        }
    }

    /// Verify the stored fingerprint matches the payload; `SnapshotCorrupt`
    /// otherwise (S5.3, Phase 5.4 pattern).
    pub fn verify(&self) -> Result<(), DataError> {
        if self.hash != self.payload.fingerprint() {
            return Err(DataError::SnapshotCorrupt(
                "shielded pool state: stored hash != payload.fingerprint()".into(),
            ));
        }
        Ok(())
    }
}

impl std::fmt::Display for SaveOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveOp::Token(_) => write!(f, "Token"),
            SaveOp::Data(_) => write!(f, "Data"),
            SaveOp::User(_) => write!(f, "User"),
            SaveOp::StakeSnapshot(_) => write!(f, "StakeSnapshot"),
            SaveOp::ExecutorSet(_) => write!(f, "ExecutorSet"),
            SaveOp::ShieldedPool(_) => write!(f, "ShieldedPool"),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct DataRequest {
    key: Vec<u8>,
    op: DataOp,
    partition_id: String
}

impl DataRequest {
    pub fn new(key: &Vec<u8>, op: DataOp, partition: &str) -> Self {
        DataRequest {
            key: key.clone(),
            op,
            partition_id: partition.to_string()
        }
    }

    // Read accessors for the RECEIVING side of the data channel. The fields
    // stay private — the wire shape is consensus surface, so these three
    // accessors are the only public read seam — but a data service cannot
    // dispatch a request it cannot read. Additive: no wire change, no
    // behavior change on the client path.
    /// The storage key the client computed for this op. For stake/executor
    /// snapshots this is the big-endian epoch; for the shielded pool,
    /// `b"shielded_pool"`; for token/user/data lookups the entity key.
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// The operation to perform.
    pub fn op(&self) -> &DataOp {
        &self.op
    }

    /// The partition the request is scoped to.
    pub fn partition_id(&self) -> &str {
        &self.partition_id
    }
}

#[derive(Debug)]
pub enum DataError {
    FromStore(String),
    SerializationError(std::io::Error),
    DeserializationError(std::io::Error),
    DataNotFound,
    StoreNotFound,
    CacheError,
    Poisoned,
    InvalidOperation(DataOp),
    InvalidSignature,
    /// Cryptographic error encountered during message processing
    CryptoError(String),
    /// The data service did not respond within the connection read/write bound
    Timeout(String),
    /// A data-channel response failed shared-secret HMAC verification
    PeerUnauthenticated(String),
    /// A persisted stake/executor snapshot failed its integrity check: the
    /// SHA-256 envelope stored alongside it does not match the re-computed
    /// digest of the payload on load, so the bytes are treated as corrupted
    /// and rejected rather than trusted (AUDIT Phase 5.4 / H9/M8).
    SnapshotCorrupt(String),
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataError::FromStore(msg) => write!(f, "FromStore({})", msg),
            DataError::SerializationError(e) => write!(f, "SerializationError({})", e),
            DataError::DeserializationError(e) => write!(f, "DeserializationError({})", e),
            DataError::DataNotFound => write!(f, "DataNotFound"),
            DataError::StoreNotFound => write!(f, "StoreNotFound"),
            DataError::CacheError => write!(f, "CacheError"),
            DataError::Poisoned => write!(f, "Poisoned"),
            DataError::InvalidOperation(op) => write!(f, "InvalidOperation({})", op),
            DataError::InvalidSignature => write!(f, "InvalidSignature"),
            DataError::CryptoError(msg) => write!(f, "CryptoError({})", msg),
            DataError::Timeout(msg) => write!(f, "Timeout({})", msg),
            DataError::PeerUnauthenticated(msg) => write!(f, "PeerUnauthenticated({})", msg),
        DataError::SnapshotCorrupt(msg) => write!(f, "SnapshotCorrupt({})", msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::sync::mpsc;

    #[test]
    fn data_error_crypto_error_display() {
        let err = DataError::CryptoError("RwLock poisoned: ...".to_string());
        assert!(err.to_string().contains("CryptoError"));
    }

    #[test]
    fn data_op_display() {
        assert_eq!(DataOp::Get(GetOp::Token).to_string(), "Get(Token)");
        assert_eq!(DataOp::Save(SaveOp::Token(Token::default())).to_string(), "Save(Token)");
    }

    #[test]
    fn get_op_display() {
        assert_eq!(GetOp::Token.to_string(), "Token");
        assert_eq!(GetOp::Data.to_string(), "Data");
        assert_eq!(GetOp::User.to_string(), "User");
    }

    #[test]
    fn save_op_display() {
        assert_eq!(SaveOp::Token(Token::default()).to_string(), "Token");
        assert_eq!(SaveOp::Data(vec![]).to_string(), "Data");
        assert_eq!(SaveOp::User(User::default()).to_string(), "User");
    }

    // Discriminator (verify a): a data service that accepts the connection but
    // never responds makes a blocking `get_user` return `Err(Timeout)` instead
    // of hanging (which is what wedged the RNS worker pool pre-fix). Reverting
    // the sender's read timeout turns this into a permanent block.
    #[test]
    fn get_user_returns_timeout_on_non_responding_data_service() {
        let temp_dir = tempfile::tempdir().unwrap();
        let sock_path = temp_dir.path().join("data.sock");
        let sock_str = sock_path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&sock_str);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        // Move a clone into the server thread so `sock_str` stays usable below.
        let server_sock = sock_str.clone();
        let server_handle = thread::spawn(move || {
            let listener = UnixListener::bind(&server_sock).unwrap();
            let _ = ready_tx.send(());
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the framed request so the client's write completes, then
                // hold the connection open WITHOUT responding.
                let mut header = [0u8; 4];
                if stream.read_exact(&mut header).is_ok() {
                    let len = u32::from_be_bytes(header) as usize;
                    let mut body = vec![0u8; len];
                    let _ = stream.read_exact(&mut body);
                }
                std::thread::sleep(Duration::from_secs(4));
            }
        });

        let _ = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread::sleep(Duration::from_millis(50));

        // Point the provider at the socket and bound reads at 1s.
        let provider = DefaultDataProvider::new()
            .with_source(ConnTarget::Local(LocalTarget::Unix(sock_str)))
            .with_timeout(Duration::from_secs(1));

        let result = provider.get_user(&vec![1u8], "default");
        assert!(
            matches!(result, Err(DataError::Timeout(_))),
            "expected Timeout on a hung data service, got {:?}",
            result
        );

        drop(server_handle);
    }
}

// ---------------------------------------------------------------------------
// StubDataProvider — in-memory DataProvider for unit tests
// ---------------------------------------------------------------------------

/// Test helper that returns pre-loaded tokens instead of connecting to
/// an external data service. Used exclusively by `#[cfg(test)]` code.
pub struct StubDataProvider {
    tokens: std::collections::HashMap<Vec<u8>, std::collections::HashMap<String, Token>>,
    users: std::sync::Mutex<std::collections::HashMap<Vec<u8>, std::collections::HashMap<String, User>>>,
    stake_snapshots: std::sync::Mutex<std::collections::HashMap<u64, StakeSnapshotEnvelope>>,
    executor_sets: std::sync::Mutex<std::collections::HashMap<u64, ExecutorSetEnvelope>>,
    pool_state: std::sync::Mutex<Option<ShieldedPoolStateEnvelope>>,
}

impl StubDataProvider {
    pub fn new() -> Self {
        StubDataProvider {
            tokens: std::collections::HashMap::new(),
            users: std::sync::Mutex::new(std::collections::HashMap::new()),
            stake_snapshots: std::sync::Mutex::new(std::collections::HashMap::new()),
            executor_sets: std::sync::Mutex::new(std::collections::HashMap::new()),
            pool_state: std::sync::Mutex::new(None),
        }
    }

    pub fn with_token(mut self, key: Vec<u8>, partition_id: String, token: Token) -> Self {
        self.tokens.entry(key).or_default().insert(partition_id, token);
        self
    }

    pub fn with_user(mut self, key: Vec<u8>, partition_id: String, user: User) -> Self {
        self.users
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .insert(partition_id, user);
        self
    }

    /// Add a stake snapshot for a given epoch. Wraps the payload in its SHA-256
    /// envelope, so a subsequent `get_stake_snapshot` verifies integrity — matching
    /// DefaultDataProvider.
    pub fn with_stake_snapshot(mut self, epoch: u64, snapshot: StakeSet) -> Self {
        let env = StakeSnapshotEnvelope::new(snapshot, epoch);
        self.stake_snapshots.lock().unwrap().insert(epoch, env);
        self
    }

    /// Add a stake snapshot with a deliberately wrong fingerprint, to prove that a
    /// corrupted/attested snapshot is detected on load (AUDIT Phase 5.4 / H9/M8).
    pub fn with_corrupted_stake_snapshot(mut self, epoch: u64, snapshot: StakeSet) -> Self {
        let mut env = StakeSnapshotEnvelope::new(snapshot, epoch);
        env.hash = [0u8; 32]; // never matches payload.fingerprint()
        self.stake_snapshots.lock().unwrap().insert(epoch, env);
        self
    }

    /// Store a shielded-pool state (wrapped in its SHA-256 envelope) so a
    /// subsequent `get_shielded_pool` verifies integrity — matching
    /// `DefaultDataProvider` (S5.3).
    pub fn with_shielded_pool(mut self, state: ShieldedPoolState) -> Self {
        let env = ShieldedPoolStateEnvelope::new(state);
        *self.pool_state.lock().unwrap() = Some(env);
        self
    }

    /// Store a shielded-pool state with a deliberately wrong fingerprint, to
    /// prove a corrupted pool state is detected on load (S5.3, Phase 5.4 pattern).
    pub fn with_corrupted_shielded_pool(mut self, state: ShieldedPoolState) -> Self {
        let mut env = ShieldedPoolStateEnvelope::new(state);
        env.hash = [0u8; 32]; // never matches payload.fingerprint()
        *self.pool_state.lock().unwrap() = Some(env);
        self
    }

    /// Add an executor set for a given epoch (wrapped in its SHA-256 envelope).
    pub fn with_executor_set(mut self, epoch: u64, set: ExecutorSet) -> Self {
        let env = ExecutorSetEnvelope::new(set, epoch);
        self.executor_sets.lock().unwrap().insert(epoch, env);
        self
    }
}

impl Default for StubDataProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DataProvider for StubDataProvider {
    fn get_token(&self, key: &Vec<u8>, partition_id: &str) -> Result<Token, DataError> {
        self.tokens
            .get(key)
            .and_then(|partitions| partitions.get(partition_id))
            .cloned()
            .ok_or(DataError::DataNotFound)
    }

    fn save_token(&self, _key: &Vec<u8>, _token: Token, _partition_id: &str) -> Result<(), DataError> {
        Ok(())
    }

    // Phase 5.3 / H3: expose the stored token's chain tip so the sentinel's
    // deterministic finalizer/shard routing reflects a real mined tip in tests.
    // Mirrors DefaultDataProvider::latest_block_hash (which reads the persisted
    // token). No existing test stores a token, so those (empty tip) are unaffected.
    fn latest_block_hash(&self, partition_id: &str) -> Result<Option<Vec<u8>>, DataError> {
        for partitions in self.tokens.values() {
            if let Some(token) = partitions.get(partition_id) {
                return Ok(Some(token.blockchain.get_current_chain_state().last_hash_in));
            }
        }
        Ok(None)
    }

    fn get_data(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Vec<u8>, DataError> {
        Err(DataError::DataNotFound)
    }

    fn save_data(&self, _key: &Vec<u8>, _data: Vec<u8>, _partition_id: &str) -> Result<(), DataError> {
        Ok(())
    }

    fn get_user(&self, key: &Vec<u8>, partition_id: &str) -> Result<User, DataError> {
        self.users
            .lock()
            .unwrap()
            .get(key)
            .and_then(|partitions| partitions.get(partition_id))
            .cloned()
            .ok_or(DataError::DataNotFound)
    }

    fn save_user(&self, key: &Vec<u8>, user: User, partition_id: &str) -> Result<(), DataError> {
        self.users
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .insert(partition_id.to_string(), user);
        Ok(())
    }

    fn get_stake_snapshot(&self, epoch: u64, _partition_id: &str) -> Result<StakeSet, DataError> {
        let env = self
            .stake_snapshots
            .lock()
            .unwrap()
            .get(&epoch)
            .cloned()
            .ok_or(DataError::DataNotFound)?;
        env.verify()?;
        Ok(env.payload)
    }

    fn save_stake_snapshot(&self, epoch: u64, snapshot: StakeSet, _partition_id: &str) -> Result<(), DataError> {
        let env = StakeSnapshotEnvelope::new(snapshot, epoch);
        self.stake_snapshots.lock().unwrap().insert(epoch, env);
        Ok(())
    }

    fn get_executor_set(&self, epoch: u64, _partition_id: &str) -> Result<ExecutorSet, DataError> {
        let env = self
            .executor_sets
            .lock()
            .unwrap()
            .get(&epoch)
            .cloned()
            .ok_or(DataError::DataNotFound)?;
        env.verify()?;
        Ok(env.payload)
    }

    fn save_executor_set(&self, epoch: u64, set: ExecutorSet, _partition_id: &str) -> Result<(), DataError> {
        let env = ExecutorSetEnvelope::new(set, epoch);
        self.executor_sets.lock().unwrap().insert(epoch, env);
        Ok(())
    }

    fn get_shielded_pool(&self, _partition_id: &str) -> Result<Option<ShieldedPoolState>, DataError> {
        let env = self.pool_state.lock().unwrap().clone().ok_or(DataError::DataNotFound)?;
        env.verify()?;
        Ok(Some(env.payload))
    }

    fn save_shielded_pool(&self, state: &ShieldedPoolState, _partition_id: &str) -> Result<(), DataError> {
        let env = ShieldedPoolStateEnvelope::new(state.clone());
        *self.pool_state.lock().unwrap() = Some(env);
        Ok(())
    }
}

// Phase 5.4 / H9+M8: a snapshot whose digest does not match its payload is rejected on load,
// never trusted. StubDataProvider exposes a valid round-trip and a deliberately corrupted one.
#[cfg(test)]
mod snapshot_envelope_tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    fn one_staker() -> StakeSet {
        StakeSet {
            stakers: [(b"alice".to_vec(), 100)].into_iter().collect(),
        }
    }

    // Regression: a stored snapshot whose SHA-256 digest does not match the payload's
    // fingerprint is detected as `SnapshotCorrupt` on load (AUDIT Phase 5.4 / H9+M8).
    // Reverting the envelope `verify()` (or the StubDataProvider/DefaultDataProvider load path
    // that calls it) makes this corrupted snapshot round-trip back as `Ok`, trusting untended
    // bytes — the test would pass on the buggy code and fail on the fix.
    #[test]
    fn snapshot_envelope_detects_corruption() {
        let dp = StubDataProvider::new()
            .with_stake_snapshot(1, one_staker())
            .with_corrupted_stake_snapshot(2, one_staker());

        // Valid round-trip: fingerprint matches, payload returned byte-identical on load.
        let loaded = dp.get_stake_snapshot(1, "token").unwrap();
        assert_eq!(loaded.canonical_bytes().unwrap(), one_staker().canonical_bytes().unwrap());

        // Corrupted fingerprint: detected, not trusted.
        assert!(
            matches!(dp.get_stake_snapshot(2, "token"), Err(DataError::SnapshotCorrupt(_))),
            "corrupted snapshot must surface as SnapshotCorrupt, got {:?}",
            dp.get_stake_snapshot(2, "token")
        );

        // Same discipline for the executor set (valid round-trips; corrupted is rejected).
        let mut exec = StdHashMap::new();
        exec.insert(b"ex1".to_vec(), 50);
        let dp2 = StubDataProvider::new().with_executor_set(1, ExecutorSet { executors: exec.clone() })
            .with_corrupted_stake_snapshot(2, one_staker());
        assert!(dp2.get_executor_set(1, "token").is_ok());
        assert!(matches!(dp2.get_stake_snapshot(2, "token"), Err(DataError::SnapshotCorrupt(_))));
    }
}

// ---------------------------------------------------------------------------
// Wire-format tests (TASKS.md test-gap tail, 10/01/2026): a fake data
// service in-process, speaking the real channel protocol end to end —
// frame `[4B BE len][auth_tag(32) || body]`, request body =
// rmp(DataRequest); the tag is HMAC-SHA256(secret, body), or 32 zero
// bytes (verifying vacuously) when no secret is configured. This is the
// wire half the StubDataProvider suite could never cover: it proves the
// provider's own serialize → frame → send → receive → deserialize path,
// including the envelope integrity discipline across a real socket.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod wire_format_tests {
    use super::*;
    use crate::conns::uds::{sign_payload, verify_payload};
    use std::collections::HashMap as StdHashMap;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Mirror of `senders::AUTH_TAG_LEN` (private there); the wire contract
    /// this suite exists to pin.
    const AUTH_TAG_LEN_T: usize = 32;

    /// Spawn a fake data service on a temp UDS path. It serves exactly
    /// `expect_connections` framed requests: each decoded [`DataRequest`] —
    /// plus whether its request tag verified under `secret` — is forwarded on
    /// the returned receiver; the reply body comes from `handler`.
    fn spawn_fake_data_service<F>(
        secret: Option<Vec<u8>>,
        expect_connections: usize,
        handler: F,
    ) -> (tempfile::TempDir, String, mpsc::Receiver<(DataRequest, bool)>)
    where
        F: Fn(&DataRequest) -> Vec<u8> + Send + 'static,
    {
        let temp_dir = tempfile::tempdir().unwrap();
        let sock_path = temp_dir.path().join("data.sock");
        let sock_str = sock_path.to_str().unwrap().to_string();
        let listener = UnixListener::bind(&sock_str).unwrap();
        let (req_tx, req_rx) = mpsc::channel();
        thread::spawn(move || {
            for _ in 0..expect_connections {
                let Ok((mut stream, _)) = listener.accept() else { return };
                // A misbehaving client fails this test via the timeout, not a hang.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut header = [0u8; 4];
                if stream.read_exact(&mut header).is_err() { return; }
                let len = u32::from_be_bytes(header) as usize;
                if len < AUTH_TAG_LEN_T { return; }
                let mut body = vec![0u8; len];
                if stream.read_exact(&mut body).is_err() { return; }
                let (tag, payload) = body.split_at(AUTH_TAG_LEN_T);
                let tag_ok = verify_payload(secret.as_deref(), tag, payload);
                let Ok(request) = deserialize_rmp_to::<DataRequest>(&payload.to_vec()) else { return };
                // Reply first so the provider's blocking read completes even
                // when the test later fails on request-shape assertions.
                let reply = handler(&request);
                let (reply_tag, reply_body) = sign_payload(secret.as_deref(), &reply);
                let mut frame = Vec::with_capacity(4 + AUTH_TAG_LEN_T + reply_body.len());
                frame.extend_from_slice(
                    &((AUTH_TAG_LEN_T + reply_body.len()) as u32).to_be_bytes(),
                );
                frame.extend_from_slice(&reply_tag);
                frame.extend_from_slice(&reply_body);
                if stream.write_all(&frame).is_err() { return; }
                let _ = req_tx.send((request, tag_ok));
            }
        });
        (temp_dir, sock_str, req_rx)
    }

    /// Provider pointed at a fake service socket. `secret` is applied BEFORE
    /// `with_timeout` because `with_secret` replaces the conn factory (which
    /// would otherwise silently drop the shorter timeout).
    fn provider_at(sock: &str, secret: Option<Vec<u8>>) -> DefaultDataProvider {
        let p = DefaultDataProvider::new()
            .with_source(ConnTarget::Local(LocalTarget::Unix(sock.to_string())));
        let p = match secret {
            Some(s) => p.with_secret(s),
            None => p,
        };
        p.with_timeout(Duration::from_secs(2))
    }

    #[test]
    fn wire_get_user_round_trip() {
        let want = User { public_key: vec![1, 2, 3], fuel_balance: 42, stake: 7, nonce: 9 };
        let (_tmp, sock, reqs) =
            spawn_fake_data_service(None, 1, move |_req| serialize_to_bytes_rmp(&want).unwrap());
        let dp = provider_at(&sock, None);

        let got = dp.get_user(&vec![9u8; 4], "token").expect("user must survive the wire");
        assert_eq!(got.public_key, vec![1, 2, 3]);
        assert_eq!(got.fuel_balance, 42);
        assert_eq!(got.stake, 7);
        assert_eq!(got.nonce, 9);

        // The request the service actually received: key, op, and partition
        // as the provider serialized them.
        let (req, tag_ok) = reqs.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(req.key, vec![9u8; 4]);
        assert_eq!(req.partition_id, "token");
        assert!(matches!(req.op, DataOp::Get(GetOp::User)));
        assert!(tag_ok, "no-secret path: zero tag verifies vacuously");
    }

    #[test]
    fn wire_save_user_carries_framed_payload() {
        let (_tmp, sock, reqs) = spawn_fake_data_service(None, 1, |_req| vec![]);
        let dp = provider_at(&sock, None);
        let user = User { public_key: vec![7], fuel_balance: 1, stake: 2, nonce: 3 };

        dp.save_user(&vec![7], user, "token").expect("save acknowledged");

        let (req, _tag_ok) = reqs.recv_timeout(Duration::from_secs(5)).unwrap();
        match req.op {
            DataOp::Save(SaveOp::User(u)) => {
                assert_eq!(u.public_key, vec![7]);
                assert_eq!(u.fuel_balance, 1);
                assert_eq!(u.stake, 2);
                assert_eq!(u.nonce, 3);
            }
            other => panic!("expected Save(User) on the wire, got {other}"),
        }
    }

    #[test]
    fn wire_get_data_round_trip() {
        let (_tmp, sock, reqs) = spawn_fake_data_service(None, 1, |_req| {
            serialize_to_bytes_rmp(&vec![1u8, 2, 3, 4]).unwrap()
        });
        let dp = provider_at(&sock, None);

        let bytes = dp.get_data(&b"k".to_vec(), "token").expect("raw bytes must round-trip");
        assert_eq!(bytes, vec![1, 2, 3, 4]);

        let (req, _) = reqs.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(req.op, DataOp::Get(GetOp::Data)));
    }

    // The envelope discipline (H9/M8) over a REAL socket: a snapshot saved
    // through the provider is replayed verbatim by the fake service and must
    // load clean — and a tampered hash must fail closed as SnapshotCorrupt,
    // never as a trusted value.
    #[test]
    fn wire_stake_snapshot_envelope_round_trip_and_corruption() {
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let cap_save = captured.clone();
        let cap_get = captured.clone();
        let (_tmp, sock, reqs) =
            spawn_fake_data_service(None, 2, move |req| match &req.op {
                DataOp::Save(SaveOp::StakeSnapshot(env)) => {
                    let bytes = serialize_to_bytes_rmp(env).unwrap();
                    *cap_save.lock().unwrap() = bytes.clone();
                    vec![] // ack
                }
                DataOp::Get(GetOp::StakeSnapshot(_)) => cap_get.lock().unwrap().clone(),
                other => panic!("unexpected op on the wire: {other}"),
            });
        let dp = provider_at(&sock, None);

        let mut stakers = StdHashMap::new();
        stakers.insert(b"alice".to_vec(), 100u64);
        stakers.insert(b"bob".to_vec(), 7u64);
        dp.save_stake_snapshot(1, StakeSet { stakers: stakers.clone() }, "token")
            .expect("stake snapshot save acknowledged");

        let loaded = dp.get_stake_snapshot(1, "token").expect("envelope must verify on load");
        assert_eq!(loaded.stakers, stakers);

        // The bytes that hit the wire are the ENVELOPE (hash + epoch present),
        // not the bare payload — assert the envelope form, then the op shape.
        let (get_req, _) = reqs.iter().skip(1).next().expect("get request captured");
        assert!(matches!(get_req.op, DataOp::Get(GetOp::StakeSnapshot(1))));

        // Corrupted hash: the fake service replays the envelope with the hash
        // zeroed — the provider must refuse it.
        let tampered = {
            let bytes = captured.lock().unwrap().clone();
            let mut env: StakeSnapshotEnvelope = deserialize_rmp_to(&bytes).unwrap();
            env.hash = [0u8; 32];
            serialize_to_bytes_rmp(&env).unwrap()
        };
        let (_tmp2, sock2, _reqs2) = spawn_fake_data_service(None, 1, move |_req| tampered.clone());
        let dp2 = provider_at(&sock2, None);
        assert!(
            matches!(dp2.get_stake_snapshot(1, "token"), Err(DataError::SnapshotCorrupt(_))),
            "a tampered envelope over the wire must fail closed"
        );
    }

    #[test]
    fn wire_shielded_pool_round_trip_and_corruption() {
        let state = ShieldedPoolState {
            root: [1u8; 32],
            leaf_count: 1,
            leaves: vec![[2u8; 32]],
            nullifiers: vec![[3u8; 32]],
            applied: vec![],
        };

        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let cap_save = captured.clone();
        let cap_get = captured.clone();
        let (_tmp, sock, _reqs) =
            spawn_fake_data_service(None, 2, move |req| match &req.op {
                DataOp::Save(SaveOp::ShieldedPool(env)) => {
                    let bytes = serialize_to_bytes_rmp(env).unwrap();
                    *cap_save.lock().unwrap() = bytes.clone();
                    vec![]
                }
                DataOp::Get(GetOp::ShieldedPool) => cap_get.lock().unwrap().clone(),
                other => panic!("unexpected op on the wire: {other}"),
            });
        let dp = provider_at(&sock, None);

        dp.save_shielded_pool(&state, "token").expect("pool save acknowledged");
        let loaded = dp
            .get_shielded_pool("token")
            .expect("pool envelope must verify on load")
            .expect("pool state present");
        assert_eq!(loaded.root, state.root);
        assert_eq!(loaded.leaf_count, 1);
        assert_eq!(loaded.leaves, state.leaves);
        assert_eq!(loaded.nullifiers, state.nullifiers);

        // S5.3 boot contract: a corrupt pool read is an Err (the committer
        // refuses to re-seed) — a tampered envelope must produce exactly that.
        let tampered = {
            let bytes = captured.lock().unwrap().clone();
            let mut env: ShieldedPoolStateEnvelope = deserialize_rmp_to(&bytes).unwrap();
            env.hash = [0u8; 32];
            serialize_to_bytes_rmp(&env).unwrap()
        };
        let (_tmp2, sock2, _reqs2) = spawn_fake_data_service(None, 1, move |_req| tampered.clone());
        let dp2 = provider_at(&sock2, None);
        assert!(
            matches!(dp2.get_shielded_pool("token"), Err(DataError::SnapshotCorrupt(_))),
            "a tampered pool envelope must fail closed, never re-seed"
        );
    }

    #[test]
    fn wire_garbage_response_is_deserialization_error() {
        let (_tmp, sock, _reqs) =
            spawn_fake_data_service(None, 1, |_req| b"\xc1 not msgpack at all".to_vec());
        let dp = provider_at(&sock, None);
        assert!(
            matches!(dp.get_user(&b"k".to_vec(), "token"), Err(DataError::DeserializationError(_))),
            "undecodable response must surface as DeserializationError"
        );
    }

    #[test]
    fn wire_hmac_authenticated_round_trip_and_tamper() {
        let secret = b"shared-data-secret".to_vec();

        // Matched secrets: authenticated round-trip, and the service-side tag
        // check passes on the provider's request frame.
        let (_tmp, sock, reqs) = {
            let secret = secret.clone();
            spawn_fake_data_service(Some(secret), 1, move |_req| {
                serialize_to_bytes_rmp(&User {
                    public_key: vec![5],
                    fuel_balance: 1,
                    stake: 1,
                    nonce: 1,
                })
                .unwrap()
            })
        };
        let dp = provider_at(&sock, Some(secret.clone()));
        let got = dp.get_user(&b"k".to_vec(), "token").expect("authenticated round-trip");
        assert_eq!(got.public_key, vec![5]);
        let (req, tag_ok) = reqs.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(tag_ok, "provider must emit a valid HMAC over the request body");
        assert!(matches!(req.op, DataOp::Get(GetOp::User)));

        // Mismatched service key: the response tag fails verification —
        // DataError::PeerUnauthenticated, not a trusted payload.
        let (_tmp2, sock2, _reqs2) = spawn_fake_data_service(
            Some(b"attacker-key".to_vec()),
            1,
            |_req| serialize_to_bytes_rmp(&User::default()).unwrap(),
        );
        let dp2 = provider_at(&sock2, Some(secret));
        assert!(
            matches!(dp2.get_user(&b"k".to_vec(), "token"), Err(DataError::PeerUnauthenticated(_))),
            "a response tagged under the wrong key must fail authentication"
        );
    }

    #[test]
    fn wire_connect_refusal_is_store_not_found_or_from_store() {
        // Nothing is listening on this path: the sender creation itself
        // succeeds, so the failure surfaces where it actually lands.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nobody-home.sock");
        let dp = provider_at(missing.to_str().unwrap(), None);
        let err = dp.get_user(&b"k".to_vec(), "token").expect_err("must fail without a service");
        assert!(
            matches!(err, DataError::StoreNotFound | DataError::FromStore(_)),
            "connect refusal must fail closed, got {err:?}"
        );
    }

    #[test]
    fn internal_op_guards_reject_mismatched_ops() {
        // The get/save internals each refuse the other's op class BEFORE any
        // I/O — defensive guards only reachable in-module, so tested here.
        let dp = DefaultDataProvider::new();
        assert!(matches!(
            dp.get_data_internal::<User>(&vec![], DataOp::Save(SaveOp::Data(vec![])), "p"),
            Err(DataError::InvalidOperation(_))
        ));
        assert!(matches!(
            dp.save_data_internal::<User>(&vec![], DataOp::Get(GetOp::User), "p"),
            Err(DataError::InvalidOperation(_))
        ));
    }
}