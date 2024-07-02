use moka::sync::Cache;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use dashmap::DashMap;
//use rocksdb::{DB};
use serde::{Deserialize, Serialize};
use crate::config;
use crate::environment::EnvironmentMetadataSpec;

pub struct DataProvider {
    cache: Cache<Vec<u8>, Arc<RwLock<Vec<u8>>>>,
    stores: Arc<DashMap<String, Box<dyn DataStore>>>
}

impl DataProvider {
    pub fn from_config(config: &config::Config) -> Self {
        let stores: Arc<DashMap<String, Box<dyn DataStore>>> = Arc::new(DashMap::new());
        stores.insert(config.main_environment_id.clone(), Box::new(RocksDbDataStore::new()));
        DataProvider {
            cache: get_cache(),
            stores
        }
    }

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
            cache: get_cache(),
            stores
        }
    }

    pub fn get_data(&self, token_key: &Vec<u8>, partition_id: &str)
        -> Result<Arc<RwLock<Vec<u8>>>, DataError> {
        if let Some(token_entry) = self.cache.get(token_key) {
            return Ok(Arc::clone(&token_entry))
        }

        let Some(store) = self.stores.get(partition_id)
            else { return Err(DataError::StoreNotFound) };

        let Some(data) = store.value().get_data(token_key)
            else { return Err(DataError::DataNotFound) };

        self.cache.insert(token_key.clone(), Arc::new(RwLock::new(data)));
        match self.cache.get(token_key) {
            Some(cached_token) => Ok(Arc::clone(&cached_token)),
            None => Err(DataError::CacheError)
        }
    }

    pub fn save_data(&self, key: Vec<u8>, data: Vec<u8>, partition_id: &str) -> Result<(), DataError> {

        self.cache.insert(key, Arc::new(RwLock::new(data)));

        Ok(())
    }
}

fn get_cache() -> Cache<Vec<u8>, Arc<RwLock<Vec<u8>>>> {
    Cache::builder()
        .time_to_idle(Duration::from_secs(30))
        .build()
}

#[derive(Serialize, Deserialize, Debug)]
pub enum DataProviderType {
    RocksDb
}

trait DataStore : Send + Sync {
    fn get_data(&self, key: &Vec<u8>) -> Option<Vec<u8>>;
    fn save_data(&self, key: Vec<u8>, data: Vec<u8>) -> Result<(), DataError>;
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
    fn get_data(&self, key: &Vec<u8>) -> Option<Vec<u8>> {
        todo!()
    }

    fn save_data(&self, key: Vec<u8>, data: Vec<u8>) -> Result<(), DataError> {
        todo!()
    }
}

pub enum DataError {
    DeserializationError,
    DataNotFound,
    StoreNotFound,
    CacheError
}