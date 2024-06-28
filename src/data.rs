use std::sync::{Arc, RwLock};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::environment::EnvironmentMetadataSpec;

pub struct DataProvider {
    cache: Arc<DashMap<Vec<u8>, Arc<RwLock<Vec<u8>>>>>,
    stores: Arc<DashMap<String, Box<dyn DataStore>>>
}

impl DataProvider {
    pub fn from_environment_spec(spec: &EnvironmentMetadataSpec) -> DataProvider {
        let stores = Arc::new(DashMap::new());
        for partition in spec.partitions.iter() {
            let store: Box<dyn DataStore> = match &partition.data_provider {
                DataProviderType::RocksDb => Box::new(RocksDbDataStore::new()),
                _ => panic!("Data provider of type \"{:?}\" is not supported", &partition.data_provider)
            };
            stores.insert(partition.id.clone(), store);
        }

        DataProvider {
            cache: Arc::new(DashMap::new()),
            stores
        }
    }

    pub fn get_data(&self, token_key: &[u8], partition_id: &str)
        -> Result<Arc<RwLock<Vec<u8>>>, DataError> {
        todo!()
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum DataProviderType {
    RocksDb
}

trait DataStore : Send + Sync {
    fn get_data(&self, key: &[u8]) -> Option<Vec<u8>>;
}

pub struct RocksDbDataStore {
    // TODO: have to wrap the actual store calls in mutexes
}

impl RocksDbDataStore {
    pub fn new() -> Self {
        RocksDbDataStore {}
    }
}

impl DataStore for RocksDbDataStore {
    fn get_data(&self, key: &[u8]) -> Option<Vec<u8>> {
        todo!()
    }
}

pub enum DataError {
    DeserializationError,
    DataNotFound,
    StoreNotFound,
    CacheError
}