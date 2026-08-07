use serde::{Deserialize, Serialize};

/// A protocol-level user with gas balance, stake, and identity.
/// Stored outside any token — accessible via `DataProvider::get_user()`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct User {
    /// User's public key
    pub public_key: Vec<u8>,
    /// Balance of fuel/gas for transaction execution (global across all tokens)
    pub fuel_balance: u64,
    /// Global stake for node eligibility (separate from gas)
    pub stake: u64,
    /// Transaction nonce (per-sender, not per-token)
    pub nonce: usize,
}

impl User {
    pub fn new(public_key: Vec<u8>) -> Self {
        User {
            public_key,
            fuel_balance: 0,
            stake: 0,
            nonce: 0,
        }
    }
}

/// Per-token account balance. Stored in `Token.asset_data` instead of `User`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Account {
    /// Owner's public key
    pub public_key: Vec<u8>,
    /// Balance in this specific token
    pub balance: u64,
}

impl Account {
    pub fn new(public_key: Vec<u8>, balance: u64) -> Self {
        Account { public_key, balance }
    }
}
