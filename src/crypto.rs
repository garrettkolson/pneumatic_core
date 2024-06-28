use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq)]
pub enum AsymCryptoProviderType {
    RSA
}

pub trait AsymCryptoProvider {
    fn encrypt(&self, data: Vec<u8>);
    fn decrypt(&self, data: Vec<u8>);
}