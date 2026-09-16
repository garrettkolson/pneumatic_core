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
    use crate::crypto::{Ed25519Provider, ED25519_SIG_LEN};
    use crate::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
    use crate::transactions::ShieldedTransaction;

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
    fn signed_message_ed25519_half_is_deterministic() {
        let identity = NodeIdentity::generate_in_memory();
        let body = vec![9, 8, 7];
        let a = Message::signed("env".into(), "Process", body.clone(), None, &identity).unwrap();
        let b = Message::signed("env".into(), "Process", body, None, &identity).unwrap();

        // Both signatures verify under the identity's Ed25519 key.
        assert!(verify(&a, &identity.ed25519.public_key().unwrap()));
        assert!(verify(&b, &identity.ed25519.public_key().unwrap()));

        // Ed25519 is deterministic (RFC 8032): identical (key, body) → identical
        // 64-byte Ed25519 signature bytes. The full hybrid signature is NOT
        // byte-identical across the two calls, though — ML-DSA-44 in this build
        // draws a fresh CSPRNG nonce per signature (secure, non-deterministic),
        // so the ML-DSA half varies. The gossiper dedups on hash(sender_key,
        // body), not on signature bytes, so this non-determinism is harmless.
        assert_eq!(&a.signature[..ED25519_SIG_LEN], &b.signature[..ED25519_SIG_LEN]);
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

    /// A `Message` carrying a `ShieldedTransaction` (Phase S3.2) round-trips over
    /// the existing length-prefixed MsgPack framing: signing, serializing,
    /// deserializing, and re-verifying preserves the action string, the body, and
    /// the `ShieldedTransaction::hash()`. The body (with ~2.3 KB PQC note
    /// ciphertexts per output) stays far under the 16 MiB frame cap — the assert
    /// guards that sizing.
    ///
    /// Discriminator: tampering one byte of the serialized message breaks either
    /// the Ed25519 envelope verification or the recovered `hash()`, so a
    /// swapped/forge body is rejected, never silently accepted.
    #[test]
    fn shielded_message_wire_roundtrip() {
        let identity = NodeIdentity::generate_in_memory();
        let shielded_tx = ShieldedTransaction {
            id: "tx-shielded-rt".to_string(),
            action: "ShieldedTransfer".to_string(),
            token_id: vec![5, 6, 7],
            nullifiers: vec![[1u8; 32], [2u8; 32]],
            commitments: vec![[3u8; 32]],
            merkle_root: [4u8; 32],
            proof: vec![8, 9, 10, 11],
            note_ciphertexts: vec![vec![200u8; 512], vec![201u8; 512]],
            fee: 0,
        };
        let body = serialize_to_bytes_rmp(&shielded_tx).expect("body serializes");

        let msg = Message::signed("env".to_string(), "SignShielded", body.clone(), None, &identity)
            .expect("message signs");

        // The whole message serialized (the length-prefixed framing).
        let raw = serialize_to_bytes_rmp(&msg).expect("message serializes");
        assert!(
            raw.len() < 16 * 1024 * 1024,
            "the shielded body must fit comfortably in one frame"
        );

        // Verify the envelope signature over the (un)serialized body.
        let public_key = identity.ed25519.public_key().unwrap();
        let verify_body = |m: &Message| {
            Ed25519Provider::generate()
                .check_signature(&m.signature, &public_key, &m.body)
                .unwrap_or(false)
        };
        assert!(verify_body(&msg));

        // Deserialize and check the action + recovered body hash are intact.
        let recovered: Message = deserialize_rmp_to(&raw).expect("message deserializes");
        assert_eq!(recovered.action, "SignShielded");
        assert!(verify_body(&recovered));
        let recovered_tx: ShieldedTransaction =
            deserialize_rmp_to(&recovered.body).expect("body deserializes");
        assert_eq!(recovered_tx.hash().unwrap(), shielded_tx.hash().unwrap());

        // Discriminator: a tampered BODY — carried under the ORIGINAL signature,
        // which bound the original body — must fail verification (and must change
        // the recovered hash), so a swapped/forge body is rejected, never
        // silently accepted. (Tampering an arbitrary *wire* byte is unreliable:
        // the middle of the frame lands in the ~2.3 KB identity signature, not
        // the body, so the body hash/signature would be unaffected.)
        let mut tampered_tx = shielded_tx.clone();
        tampered_tx.nullifiers[0][0] ^= 0xFF;
        let tampered_body = serialize_to_bytes_rmp(&tampered_tx).expect("tampered body serializes");
        let tampered_msg = Message {
            chain_id: "env".to_string(),
            action: "SignShielded".to_string(),
            body: tampered_body,
            signature: msg.signature.clone(),
            public_key: msg.public_key.clone(),
            stake_set: None,
        };
        assert!(
            !verify_body(&tampered_msg),
            "a tampered body must fail signature verification"
        );
        let recovered_tampered: ShieldedTransaction =
            deserialize_rmp_to(&tampered_msg.body).unwrap();
        assert_ne!(
            recovered_tampered.hash().unwrap(),
            shielded_tx.hash().unwrap(),
            "a tampered body must produce a different hash"
        );
    }
}
