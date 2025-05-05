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
use crate::tokens::Token;

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
}

#[derive(Serialize, Deserialize, Debug)]
pub enum DataOp {
    Get(GetOp),
    Save(SaveOp)
}

#[derive(Serialize, Deserialize, Debug)]
pub enum GetOp {
    Token,
    Data
}

#[derive(Serialize, Deserialize, Debug)]
pub enum SaveOp {
    Token(Token),
    Data(Vec<u8>)
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
    InvalidOperation(DataOp)
}