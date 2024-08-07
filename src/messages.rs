use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Message {
    pub env_id: String,
    pub action: String,
    pub body: Vec<u8>,
    pub signature: Vec<u8>,
    pub public_key: Vec<u8>
}

pub struct MessageBody {
    // TODO: implement this
}

pub fn acknowledge() -> Vec<u8> {
    Vec::from(b"ack")
}

pub fn reject() -> Vec<u8> {
    Vec::from(b"rej")
}