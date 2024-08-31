use std::ops::Deref;
use moka::sync::Cache;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use rocksdb::{DBWithThreadMode, MultiThreaded, Options};
use crate::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use crate::tokens::Token;

pub trait DataProvider : Send + Sync {
    fn get_token(&self, key: &Vec<u8>, partition_id: &str) -> Result<Arc<RwLock<Token>>, DataError> {
        DefaultDataProvider::get_token(key, partition_id)
    }

    fn save_token(&self, key: &Vec<u8>, token_ref: Arc<RwLock<Token>>, partition_id: &str)
                  -> Result<(), DataError> {
        DefaultDataProvider::save_token(key, token_ref, partition_id)
    }

    fn get_data(&self, key: &Vec<u8>, partition_id: &str) -> Result<Arc<RwLock<Vec<u8>>>, DataError> {
        DefaultDataProvider::get_data(key, partition_id)
    }

    fn save_data(&self, key: &Vec<u8>, data: Vec<u8>, partition_id: &str) -> Result<(), DataError> {
        DefaultDataProvider::save_data(key, data, partition_id)
    }
}

pub struct DefaultDataProvider { }

impl DataProvider for DefaultDataProvider {}

impl DefaultDataProvider {
    pub fn get_token(key: &Vec<u8>, partition_id: &str) -> Result<Arc<RwLock<Token>>, DataError> {
        let cache = Self::get_token_cache();
        if let Some(token_entry) = cache.get(key) { return Ok(token_entry.clone()); }

        let token = Self::get_token_from_db(key, partition_id)?;
        Self::put_in_token_cache(key, Arc::new(RwLock::new(token)));
        cache.get(key).ok_or(DataError::CacheError)
    }

    pub fn save_token(key: &Vec<u8>, token_ref: Arc<RwLock<Token>>, partition_id: &str)
                      -> Result<(), DataError> {
        let db = Self::get_db_factory().get_db(partition_id)?;
        let _ = db.save_token(key, &token_ref)?;
        Self::put_in_token_cache(key, token_ref);
        Ok(())
    }

    pub fn get_data(key: &Vec<u8>, partition_id: &str)
        -> Result<Arc<RwLock<Vec<u8>>>, DataError> {
        let cache = Self::get_data_cache();
        if let Some(data_entry) = cache.get(key) { return Ok(data_entry.clone()); }

        let db = Self::get_db_factory().get_db(partition_id)?;
        let data = db.get_data(key)?;
        Self::put_in_data_cache(key, Arc::new(RwLock::new(data)));
        cache.get(key).or_or(DataError::CacheError)
    }

    pub fn save_data(key: &Vec<u8>, data: Vec<u8>, partition_id: &str) -> Result<(), DataError> {
        let db = Self::get_db_factory().get_db(partition_id)?;
        let _ = db.save_data(key, &data)?;
        Self::put_in_data_cache(key, Arc::new(RwLock::new(data)));
        Ok(())
    }

    pub fn save_typed_data<T: serde::Serialize>(key: &Vec<u8>, data: &T, partition_id: &str) -> Result<(), DataError> {
        let db = Self::get_db_factory().get_db(partition_id)?;
        let Ok(serialized) = serialize_to_bytes_rmp(data)
            else { return Err(DataError::SerializationError) };

        let _ = db.save_data(key, &serialized)?;
        Self::put_in_data_cache(key, Arc::new(RwLock::new(serialized)));
        Ok(())
    }

    pub fn save_locked_data<T: serde::Serialize>(key: &Vec<u8>, data: Arc<RwLock<T>>, partition_id: &str)
                    -> Result<(), DataError> {
        let db = Self::get_db_factory().get_db(partition_id)?;
        let Ok(write_data) = data.write()
            else { return Err(DataError::Poisoned) };
        let Ok(serialized) = serialize_to_bytes_rmp(write_data.deref())
            else { return Err(DataError::SerializationError) };

        let _ = db.save_data(key, &serialized)?;
        Self::put_in_data_cache(key, data.clone());
        Ok(())
    }

    fn get_token_from_db(key: &Vec<u8>, partition_id: &str) -> Result<Token, DataError> {
        let db = Self::get_db_factory().get_db(partition_id)?;
        db.get_token(key)
    }

    fn put_in_token_cache(key: &Vec<u8>, data: Arc<RwLock<Token>>) {
        Self::get_token_cache().insert(key.clone(), data)
    }

    fn put_in_data_cache(key: &Vec<u8>, data: Arc<RwLock<Vec<u8>>>) {
        Self::get_data_cache().insert(key.clone(), data)
    }

    fn get_token_cache() -> &'static TokenCache {
        TOKEN_CACHE.get_or_init(|| get_token_cache())
    }

    fn get_data_cache() -> &'static DataCache {
        DATA_CACHE.get_or_init(|| get_data_cache())
    }

    fn get_db_factory() -> &'static Box<dyn DbFactory> {
        DB_FACTORY.get_or_init(|| get_db_factory())
    }
}

//////////////////// Globals ///////////////////////

static TOKEN_CACHE: OnceLock<TokenCache> = OnceLock::new();
static DATA_CACHE: OnceLock<DataCache> = OnceLock::new();
static DB_FACTORY: OnceLock<Box<dyn DbFactory>> = OnceLock::new();

fn get_token_cache() -> TokenCache {
    // TODO: replace this with config.json call or something
    Cache::builder()
        .time_to_idle(Duration::from_secs(30))
        .build()
}

fn get_data_cache() -> DataCache {
    // TODO: replace this with config.json call or something
    Cache::builder()
        .time_to_idle(Duration::from_secs(30))
        .build()
}

fn get_db_factory() -> Box<dyn DbFactory> {
    // TODO: replace this with config.json call or something (per partition_id?)
    Box::new(RocksDbFactory { })
}

////////////// Data Factories/Stores ////////////////

trait Db {
    fn get_token(&self, key: &Vec<u8>) -> Result<Token, DataError>;
    fn save_token(&self, key: &Vec<u8>, token: &Arc<RwLock<Token>>) -> Result<(), DataError>;
    fn get_data(&self, key: &Vec<u8>) -> Result<Vec<u8>, DataError>;
    fn save_data(&self, key: &Vec<u8>, data: &Vec<u8>) -> Result<(), DataError>;
}

trait DbFactory : Send + Sync {
    fn get_db(&self, partition_id: &str) -> Result<Box<dyn Db>, DataError>;
}

struct RocksDbFactory { }

impl DbFactory for RocksDbFactory {
    fn get_db(&self, partition_id: &str) -> Result<Box<dyn Db>, DataError> {
        let db = RocksDb::new(partition_id)?;
        Ok(Box::new(db))
    }
}

struct RocksDb {
    store: DBWithThreadMode<MultiThreaded>
}

impl RocksDb {
    fn new(partition_id: &str) -> Result<Self, DataError> {
        match DBWithThreadMode::open(&Self::with_options(), partition_id) {
            Err(err) => Err(DataError::FromStore(err.into_string())),
            Ok(db) => {
                let rocks_db = RocksDb { store: db };
                Ok(rocks_db)
            }
        }
    }

    fn with_options() -> Options {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts
    }
}

impl Db for RocksDb {
    fn get_token(&self, key: &Vec<u8>) -> Result<Token, DataError> {
        match self.store.get(key) {
            Err(e) => Err(DataError::FromStore(e.into_string())),
            Ok(None) => Err(DataError::DataNotFound),
            Ok(Some(data)) => {
                match deserialize_rmp_to::<Token>(&data) {
                    Err(_) => Err(DataError::DeserializationError),
                    Ok(token) => Ok(token)
                }
            }
        }
    }

    fn save_token(&self, key: &Vec<u8>, token_ref: &Arc<RwLock<Token>>) -> Result<(), DataError> {
        let Ok(token) = token_ref.write()
            else { return Err(DataError::Poisoned) };

        let Ok(data) = serialize_to_bytes_rmp(token.deref())
            else { return Err(DataError::SerializationError) };

        self.save_data(key, &data)
    }

    fn get_data(&self, key: &Vec<u8>) -> Result<Vec<u8>, DataError> {
        match self.store.get(key) {
            Err(e) => Err(DataError::FromStore(e.into_string())),
            Ok(None) => Err(DataError::DataNotFound),
            Ok(Some(data)) => Ok(data)
        }
    }

    fn save_data(&self, key: &Vec<u8>, data: &Vec<u8>) -> Result<(), DataError> {
        match self.store.put(key, data) {
            Err(err) => Err(DataError::FromStore(err.into_string())),
            Ok(_) => Ok(())
        }
    }
}

type TokenCache = Cache<Vec<u8>, Arc<RwLock<Token>>>;
type DataCache = Cache<Vec<u8>, Arc<RwLock<Vec<u8>>>>;

#[derive(Debug)]
pub enum DataError {
    FromStore(String),
    SerializationError,
    DeserializationError,
    DataNotFound,
    StoreNotFound,
    CacheError,
    Poisoned
}