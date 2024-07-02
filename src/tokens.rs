use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::encoding;

#[derive(Serialize, Deserialize)]
pub struct Token {
    pub metadata: HashMap<String, String>,
    asset_data: Option<Vec<u8>>
}

impl Token {
    pub fn new() -> Token {
        Token {
            metadata: HashMap::new(),
            asset_data: None
        }
    }

    pub fn from_asset<T>(asset: &T) -> Result<Token, std::io::Error>
        where T : Serialize
    {
        match encoding::serialize_to_bytes_rmp(asset) {
            Ok(data) => {
                Ok(Token {
                    metadata: HashMap::new(),
                    asset_data: Some(data)
                })
            },
            Err(error) => Err(error)
        }
    }

    pub fn set_metadata(&mut self, key: String, value: String) {
        todo!()
        //self.metadata.insert(key, value);
    }

    pub fn get_asset<T>(&self) -> Option<T>
        where T : for<'a> Deserialize<'a>
    {
        let Some(asset) = &self.asset_data else { return None };
        match encoding::deserialize_rmp_to::<T>(asset) {
            Ok(a) => Some(a),
            Err(_) => None
        }
    }

    pub fn get_asset_mut<T>(&self) -> Option<T>
        where T : for<'a> Deserialize<'a>
    {
        todo!()
    }
}