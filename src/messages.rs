use serde::{Deserialize, Serialize};

/// Wire-format message between services.
/// `chain_id` identifies the environment/token blockchain.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Message {
    /// Target environment / token blockchain identifier
    pub chain_id: String,
    /// Action to perform (e.g., "Process", "Confirm", "Reject", "Register")
    pub action: String,
    /// MsgPack-serialized action body
    pub body: Vec<u8>,
    /// Signature over the message body
    pub signature: Vec<u8>,
    /// Public key of the sender
    pub public_key: Vec<u8>,
}

/// Generic typed message body for parameterized actions.
/// The body field contains a MsgPack-serialized value of type T.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MessageBody<T> {
    /// The action name (e.g., "ProcessTransaction", "ConfirmBlock")
    pub action: String,
    /// The typed payload
    pub body: T,
}

/// Returns a MsgPack-serialized acknowledgement payload.
pub fn acknowledge() -> Vec<u8> {
    Vec::from(b"ack")
}

/// Returns a MsgPack-serialized rejection payload.
pub fn reject() -> Vec<u8> {
    Vec::from(b"rej")
}
