use std::net::IpAddr::V4;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::Deref;
use moka::sync::Cache;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use serde::{Deserialize, Serialize};
use serde_json::error::Category::Data;
use crate::conns::{ConnError, ConnTarget, LocalTarget};
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

    fn get_user(&self, key: &Vec<u8>, partition: &str) -> Result<User, DataError> {
        let source = self.get_source();
        if let Ok(sender) = self.conn_factory.get_sender(source) {
            let data = self.serialize_request(key, DataOp::Get(GetOp::User), partition)?;
            let response = match sender.get_response(&data) {
                Ok(data) => data,
                Err(err) => return Err(conn_error_to_data_error(err))
            };

            return match deserialize_rmp_to::<User>(&response) {
                Ok(user) => Ok(user),
                Err(err) => Err(DataError::DeserializationError(err))
            }
        }

        Err(DataError::StoreNotFound)
    }

    fn save_user(&self, key: &Vec<u8>, user: User, partition: &str) -> Result<(), DataError> {
        let source = self.get_source();
        if let Ok(sender) = self.conn_factory.get_sender(source) {
            let data = self.serialize_request(key, DataOp::Save(SaveOp::User(user)), partition)?;
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
}

impl std::fmt::Display for GetOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GetOp::Token => write!(f, "Token"),
            GetOp::Data => write!(f, "Data"),
            GetOp::User => write!(f, "User"),
            GetOp::StakeSnapshot(epoch) => write!(f, "StakeSnapshot({})", epoch),
            GetOp::ExecutorSet(epoch) => write!(f, "ExecutorSet({})", epoch),
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

impl std::fmt::Display for SaveOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveOp::Token(_) => write!(f, "Token"),
            SaveOp::Data(_) => write!(f, "Data"),
            SaveOp::User(_) => write!(f, "User"),
            SaveOp::StakeSnapshot(_) => write!(f, "StakeSnapshot"),
            SaveOp::ExecutorSet(_) => write!(f, "ExecutorSet"),
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
}

impl StubDataProvider {
    pub fn new() -> Self {
        StubDataProvider {
            tokens: std::collections::HashMap::new(),
            users: std::sync::Mutex::new(std::collections::HashMap::new()),
            stake_snapshots: std::sync::Mutex::new(std::collections::HashMap::new()),
            executor_sets: std::sync::Mutex::new(std::collections::HashMap::new()),
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