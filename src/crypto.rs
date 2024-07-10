use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq)]
pub enum AsymCryptoProviderType {
    RSA
}

pub fn get_asym_provider(provider_type: &AsymCryptoProviderType) -> impl AsymCryptoProvider {
    match provider_type {
        AsymCryptoProviderType::RSA => RsaCryptoProvider::init()
    }
}

pub trait AsymCryptoProvider {
    fn encrypt(&self, data: Vec<u8>);
    fn decrypt(&self, data: Vec<u8>);
}

pub struct RsaCryptoProvider {

}

impl RsaCryptoProvider {
    fn init() -> Self {
        RsaCryptoProvider {}
    }
}

impl AsymCryptoProvider for RsaCryptoProvider {
    fn encrypt(&self, data: Vec<u8>) {
        todo!()
    }

    fn decrypt(&self, data: Vec<u8>) {
        todo!()
    }
}