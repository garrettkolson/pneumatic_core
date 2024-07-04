use std::ops::Deref;
use moka::sync::Cache;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use rocksdb::{DBWithThreadMode, MultiThreaded, Options};
use crate::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use crate::tokens::Token;

pub struct DataProvider { }

impl DataProvider {
    pub fn get_token(key: &Vec<u8>, partition_id: &str) -> Result<Arc<RwLock<Token>>, DataError> {
        let cache = Self::get_cache();
        if let Some(token_entry) = cache.get(key) { return Ok(token_entry.clone()); }

        let token = Self::get_token_from_db(key, partition_id)?;
        Self::put_in_cache(key, Arc::new(RwLock::new(token)));
        cache.get(key).ok_or(DataError::CacheError)
    }

    pub fn save_token(key: &Vec<u8>, token_ref: Arc<RwLock<Token>>, partition_id: &str)
                      -> Result<(), DataError> {
        let db = Self::get_db_factory().get_db(partition_id)?;
        let _ = db.save_token(key, &token_ref)?;
        Self::put_in_cache(key, token_ref);
        Ok(())
    }

    fn get_token_from_db(key: &Vec<u8>, partition_id: &str) -> Result<Token, DataError> {
        let db = Self::get_db_factory().get_db(partition_id)?;
        db.get_token(key)
    }

    fn put_in_cache(key: &Vec<u8>, token: Arc<RwLock<Token>>) {
        Self::get_cache().insert(key.clone(), token)
    }

    fn get_cache() -> &'static TokenCache {
        CACHE.get_or_init(|| get_cache())
    }

    fn get_db_factory() -> &'static Box<dyn DbFactory> {
        DB_FACTORY.get_or_init(|| get_db_factory())
    }
}

//////////////////// Globals ///////////////////////

static CACHE: OnceLock<TokenCache> = OnceLock::new();
static DB_FACTORY: OnceLock<Box<dyn DbFactory>> = OnceLock::new();

fn get_cache() -> TokenCache {
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

        match self.store.put(key, data) {
            Err(err) => Err(DataError::FromStore(err.into_string())),
            Ok(_) => Ok(())
        }
    }
}

type TokenCache = Cache<Vec<u8>, Arc<RwLock<Token>>>;

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