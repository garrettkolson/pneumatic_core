use std::net::IpAddr::V4;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::Deref;
use moka::sync::Cache;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use serde::{Deserialize, Serialize};
use serde_json::error::Category::Data;
use crate::conns::{ConnTarget, LocalTarget};
use crate::conns::factories::{ConnFactory, IsConnFactory};
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
}

pub struct DefaultDataProvider {
    conn_factory: ConnFactory
}

impl DefaultDataProvider {
    pub fn new() -> Self {
        DefaultDataProvider {
            conn_factory: ConnFactory::new()
        }
    }

    pub fn get_source() -> ConnTarget {
        let local_target = match cfg!(unix) {
            true => LocalTarget::Unix(DATA_UNIX_PATH.to_string()),
            false => LocalTarget::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, DATA_TCP_PORT)))
        };

        ConnTarget::Local(local_target)
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
        let source = Self::get_source();
        if let Ok(sender) = self.conn_factory.get_sender(source) {
            let data = self.serialize_request(key, op, partition)?;
            let response = match sender.get_response(&data) {
                Ok(data) => data,
                Err(err) => return Err(DataError::FromStore(err.to_string()))
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
        let source = Self::get_source();
        if let Ok(sender) = self.conn_factory.get_sender(source) {
            let data = self.serialize_request(key, op, partition)?;
            return match sender.get_response(&data) {
                Ok(_) => Ok(()),
                Err(err) => Err(DataError::FromStore(err.to_string()))
            };
        }

        Err(DataError::StoreNotFound)
    }

    fn get_user(&self, key: &Vec<u8>, partition: &str) -> Result<User, DataError> {
        let source = Self::get_source();
        if let Ok(sender) = self.conn_factory.get_sender(source) {
            let data = self.serialize_request(key, DataOp::Get(GetOp::User), partition)?;
            let response = match sender.get_response(&data) {
                Ok(data) => data,
                Err(err) => return Err(DataError::FromStore(err.to_string()))
            };

            return match deserialize_rmp_to::<User>(&response) {
                Ok(user) => Ok(user),
                Err(err) => Err(DataError::DeserializationError(err))
            }
        }

        Err(DataError::StoreNotFound)
    }

    fn save_user(&self, key: &Vec<u8>, user: User, partition: &str) -> Result<(), DataError> {
        let source = Self::get_source();
        if let Ok(sender) = self.conn_factory.get_sender(source) {
            let data = self.serialize_request(key, DataOp::Save(SaveOp::User(user)), partition)?;
            return match sender.get_response(&data) {
                Ok(_) => Ok(()),
                Err(err) => Err(DataError::FromStore(err.to_string()))
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
        self.get_data_internal::<StakeSet>(&epoch.to_be_bytes().to_vec(), DataOp::Get(GetOp::StakeSnapshot(epoch)), partition_id)
    }

    fn save_stake_snapshot(&self, epoch: u64, snapshot: StakeSet, partition_id: &str) -> Result<(), DataError> {
        self.save_data_internal::<StakeSet>(&epoch.to_be_bytes().to_vec(), DataOp::Save(SaveOp::StakeSnapshot(snapshot)), partition_id)
    }

    fn get_executor_set(&self, epoch: u64, partition_id: &str) -> Result<ExecutorSet, DataError> {
        self.get_data_internal::<ExecutorSet>(&epoch.to_be_bytes().to_vec(), DataOp::Get(GetOp::ExecutorSet(epoch)), partition_id)
    }

    fn save_executor_set(&self, epoch: u64, set: ExecutorSet, partition_id: &str) -> Result<(), DataError> {
        self.save_data_internal::<ExecutorSet>(&epoch.to_be_bytes().to_vec(), DataOp::Save(SaveOp::ExecutorSet(set)), partition_id)
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
    StakeSnapshot(StakeSet),
    ExecutorSet(ExecutorSet),
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

// ---------------------------------------------------------------------------
// StubDataProvider — in-memory DataProvider for unit tests
// ---------------------------------------------------------------------------

/// Test helper that returns pre-loaded tokens instead of connecting to
/// an external data service. Used exclusively by `#[cfg(test)]` code.
pub struct StubDataProvider {
    tokens: std::collections::HashMap<Vec<u8>, std::collections::HashMap<String, Token>>,
    users: std::sync::Mutex<std::collections::HashMap<Vec<u8>, std::collections::HashMap<String, User>>>,
    stake_snapshots: std::sync::Mutex<std::collections::HashMap<u64, StakeSet>>,
    executor_sets: std::sync::Mutex<std::collections::HashMap<u64, ExecutorSet>>,
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

    /// Add a stake snapshot for a given epoch.
    pub fn with_stake_snapshot(mut self, epoch: u64, snapshot: StakeSet) -> Self {
        self.stake_snapshots
            .lock()
            .unwrap()
            .insert(epoch, snapshot);
        self
    }

    /// Add an executor set for a given epoch.
    pub fn with_executor_set(mut self, epoch: u64, set: ExecutorSet) -> Self {
        self.executor_sets
            .lock()
            .unwrap()
            .insert(epoch, set);
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
        self.stake_snapshots
            .lock()
            .unwrap()
            .get(&epoch)
            .cloned()
            .ok_or(DataError::DataNotFound)
    }

    fn save_stake_snapshot(&self, epoch: u64, snapshot: StakeSet, _partition_id: &str) -> Result<(), DataError> {
        self.stake_snapshots
            .lock()
            .unwrap()
            .insert(epoch, snapshot);
        Ok(())
    }

    fn get_executor_set(&self, epoch: u64, _partition_id: &str) -> Result<ExecutorSet, DataError> {
        self.executor_sets
            .lock()
            .unwrap()
            .get(&epoch)
            .cloned()
            .ok_or(DataError::DataNotFound)
    }

    fn save_executor_set(&self, epoch: u64, set: ExecutorSet, _partition_id: &str) -> Result<(), DataError> {
        self.executor_sets
            .lock()
            .unwrap()
            .insert(epoch, set);
        Ok(())
    }
}