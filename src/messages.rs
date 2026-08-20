use serde::{Deserialize, Serialize};

use crate::crypto::AsymCryptoProvider;
use crate::epoch::StakeSet;
use crate::errors::PneumaticError;
use crate::rns::identity::NodeIdentity;

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
    /// Stake set for quorum gossip — populated only on "BlockFinalized" messages.
    /// Enables receiving nodes to perform stake-weighted confirmation tracking.
    #[serde(default)]
    pub stake_set: Option<StakeSet>,
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

impl Message {
    /// Build an outgoing message and sign its body with the node's Ed25519 identity.
    ///
    /// The signed payload is the raw `body` bytes, and `public_key` is set to the
    /// identity's verifying key — the same key the node registers under. Receivers
    /// verify with `check_signature(signature, public_key, body)` (see
    /// `Gossiper::handle_message` in src/gossiper.rs); the signed payload and the
    /// verified payload must stay identical, so any change here requires a
    /// matching change on the verifying side.
    pub fn signed(
        chain_id: String,
        action: &str,
        body: Vec<u8>,
        stake_set: Option<StakeSet>,
        identity: &NodeIdentity,
    ) -> Result<Self, PneumaticError> {
        Ok(Message {
            signature: identity.sign_message(&body)?,
            public_key: identity.ed25519.public_key()?,
            chain_id,
            action: action.to_string(),
            body,
            stake_set,
        })
    }
}

/// Returns a MsgPack-serialized acknowledgement payload.
pub fn acknowledge() -> Vec<u8> {
    Vec::from(b"ack")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Ed25519Provider;

    fn verify(message: &Message, expected_public_key: &[u8]) -> bool {
        Ed25519Provider::generate()
            .check_signature(&message.signature, expected_public_key, &message.body)
            .unwrap_or(false)
    }

    #[test]
    fn signed_message_verifies_under_identity_key() {
        let identity = NodeIdentity::generate_in_memory();
        let body = vec![1, 2, 3, 4];
        let msg = Message::signed("env".into(), "Process", body.clone(), None, &identity).unwrap();

        let public_key = identity.ed25519.public_key().unwrap();
        assert_eq!(msg.public_key, public_key);
        assert!(!msg.signature.is_empty());
        assert!(verify(&msg, &public_key));
    }

    #[test]
    fn signed_message_is_deterministic_for_same_identity_and_body() {
        let identity = NodeIdentity::generate_in_memory();
        let body = vec![9, 8, 7];
        let a = Message::signed("env".into(), "Process", body.clone(), None, &identity).unwrap();
        let b = Message::signed("env".into(), "Process", body, None, &identity).unwrap();

        // Deterministic Ed25519: identical (key, body) → identical signature bytes.
        // The gossiper's signature-byte dedup relies on this.
        assert_eq!(a.signature, b.signature);
    }

    #[test]
    fn signed_message_differs_across_identities() {
        let a = NodeIdentity::generate_in_memory();
        let b = NodeIdentity::generate_in_memory();
        let body = vec![1, 1, 1];
        let ma = Message::signed("env".into(), "Process", body.clone(), None, &a).unwrap();
        let mb = Message::signed("env".into(), "Process", body, None, &b).unwrap();

        assert_ne!(ma.signature, mb.signature);
        // Each signature verifies under its own key only.
        let pb = b.ed25519.public_key().unwrap();
        assert!(!verify(&ma, &pb));
    }

    #[test]
    fn signed_message_rejects_tampered_body() {
        let identity = NodeIdentity::generate_in_memory();
        let public_key = identity.ed25519.public_key().unwrap();
        let mut msg =
            Message::signed("env".into(), "Process", vec![5, 5, 5], None, &identity).unwrap();
        msg.body.push(6);

        assert!(!verify(&msg, &public_key));
    }

    #[test]
    fn signed_message_preserves_stake_set() {
        let identity = NodeIdentity::generate_in_memory();
        let stake_set = Some(crate::epoch::StakeSet {
            stakers: std::collections::HashMap::from([(vec![1u8], 100)]),
        });
        let msg = Message::signed(
            "env".into(),
            "BlockFinalized",
            vec![1],
            stake_set.clone(),
            &identity,
        )
        .unwrap();

        assert!(msg.stake_set.is_some());
        assert!(verify(&msg, &identity.ed25519.public_key().unwrap()));
    }
}
