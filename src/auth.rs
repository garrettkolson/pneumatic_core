//! C1 envelope authentication primitive: verify the envelope signature and
//! resolve the sender's registered role set — fail closed on either step.
//!
//! The worker crates' inbound handlers (finalizer executor votes, finalizer
//! shielded arms, committer action router) all repeat the same opening move
//! before applying their *local* role policy: verify the envelope signature
//! over `body` against the claimed `public_key`, then require the proven key
//! to be registered under some role. This module owns those two shared steps;
//! what stays per-crate is the role gate — which resolved roles may perform
//! which action — along with each crate's exact error taxonomy.
//!
//! The load-bearing subtlety, spelled out at every original site:
//! [`AsymCryptoProvider::check_signature`] returns `Ok(false)` — not `Err` —
//! on a signature mismatch, so a bare `?` would silently *accept* a failing
//! verification. This helper negates the flag itself; callers can no longer
//! get that wrong.

use crate::crypto::AsymCryptoProvider;
use crate::node::registry::NodeRegistry;
use crate::node::NodeRegistryType;

/// Why an envelope sender failed C1 authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeAuthError {
    /// The envelope signature did not verify against the claimed key.
    /// `reason` is `Some(..)` when the provider itself errored and `None`
    /// for a clean `Ok(false)` mismatch.
    Signature {
        public_key: Vec<u8>,
        reason: Option<String>,
    },
    /// The envelope verified, but the proven key is registered under no
    /// role — an unknown node.
    Unregistered { public_key: Vec<u8> },
}

/// Verify an envelope signature and resolve the sender's registered roles.
///
/// On success returns the sender's **full** role set (a composite identity
/// registered under several types gets all of them); the caller applies its
/// own gate to that set. Fails closed: a provider error, an `Ok(false)`
/// mismatch, or an empty role set each return [`EnvelopeAuthError`] without
/// revealing which step succeeded beyond what the caller needs to format.
pub fn authenticate_envelope(
    crypto: &dyn AsymCryptoProvider,
    registry: &NodeRegistry,
    signature: &[u8],
    public_key: &[u8],
    body: &[u8],
) -> Result<Vec<NodeRegistryType>, EnvelopeAuthError> {
    let verified = crypto
        .check_signature(signature, public_key, body)
        .map_err(|e| EnvelopeAuthError::Signature {
            public_key: public_key.to_vec(),
            reason: Some(e.to_string()),
        })?;
    if !verified {
        return Err(EnvelopeAuthError::Signature {
            public_key: public_key.to_vec(),
            reason: None,
        });
    }
    let roles = registry.find_node_types_by_public_key(public_key);
    if roles.is_empty() {
        return Err(EnvelopeAuthError::Unregistered {
            public_key: public_key.to_vec(),
        });
    }
    Ok(roles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::node::{NodeRegistryType, NodeTypeConfig};
    use crate::rns::identity::NodeIdentity;
    use dashmap::DashMap;
    use std::sync::Arc;

    fn registry_with(types: &[(NodeRegistryType, usize)]) -> NodeRegistry {
        let type_configs = DashMap::new();
        for (t, max) in types {
            type_configs.insert(
                t.clone(),
                NodeTypeConfig { min: 0, max: *max, min_stake: 0 },
            );
        }
        let config = Arc::new(Config::new_for_testing(
            "test_env".to_string(),
            Arc::new(DashMap::new()),
            Arc::new(type_configs),
        ));
        NodeRegistry::init(config, None, Arc::new(|_, _| true))
    }

    /// Drive the real `Register` path so the identity lands in the registry
    /// with a valid binding, exactly as a live peer would.
    fn register(
        reg: &NodeRegistry,
        identity: &NodeIdentity,
        requested_type: NodeRegistryType,
        types: Vec<NodeRegistryType>,
    ) -> Vec<u8> {
        let key = identity.ed25519.public_key().unwrap();
        let binding = identity
            .sign_binding(&identity.rhash, &requested_type, &types)
            .expect("sign binding");
        reg.handle_register(crate::node::NodeRequest {
            requester_key: key.clone(),
            requester_rhash: identity.rhash,
            request_type: crate::node::NodeRequestType::Register,
            requester_types: types,
            requested_type: requested_type,
            binding_signature: binding,
        });
        key
    }

    fn signed_body(identity: &NodeIdentity, body: &[u8]) -> Vec<u8> {
        identity.sign_message(body).expect("sign body")
    }

    #[test]
    fn authenticates_registered_signer_and_returns_its_roles() {
        let reg = registry_with(&[(NodeRegistryType::Executor, 5)]);
        let identity = NodeIdentity::generate_in_memory();
        let key = register(
            &reg,
            &identity,
            NodeRegistryType::Executor,
            vec![NodeRegistryType::Executor],
        );
        let body = b"vote-payload";
        let sig = signed_body(&identity, body);

        let roles =
            authenticate_envelope(&identity.ed25519, &reg, &sig, &key, body).expect("authenticates");
        assert_eq!(roles, vec![NodeRegistryType::Executor]);
    }

    #[test]
    fn composite_identity_resolves_its_full_role_set() {
        let reg = registry_with(&[
            (NodeRegistryType::Executor, 5),
            (NodeRegistryType::Finalizer, 5),
        ]);
        let identity = NodeIdentity::generate_in_memory();
        let key = register(
            &reg,
            &identity,
            NodeRegistryType::Executor,
            vec![NodeRegistryType::Executor, NodeRegistryType::Finalizer],
        );
        let body = b"vote-payload";
        let sig = signed_body(&identity, body);

        let roles =
            authenticate_envelope(&identity.ed25519, &reg, &sig, &key, body).expect("authenticates");
        assert!(roles.contains(&NodeRegistryType::Executor));
        assert!(roles.contains(&NodeRegistryType::Finalizer));
    }

    #[test]
    fn tampered_body_fails_closed_with_a_clean_mismatch() {
        let reg = registry_with(&[(NodeRegistryType::Executor, 5)]);
        let identity = NodeIdentity::generate_in_memory();
        let key = register(
            &reg,
            &identity,
            NodeRegistryType::Executor,
            vec![NodeRegistryType::Executor],
        );
        let sig = signed_body(&identity, b"original-body");

        let err = authenticate_envelope(&identity.ed25519, &reg, &sig, &key, b"tampered-body")
            .expect_err("tampered body must fail");
        assert_eq!(
            err,
            EnvelopeAuthError::Signature {
                public_key: key,
                reason: None
            }
        );
    }

    #[test]
    fn signature_claimed_under_a_different_key_fails_closed() {
        let reg = registry_with(&[(NodeRegistryType::Executor, 5)]);
        let signer = NodeIdentity::generate_in_memory();
        let victim = NodeIdentity::generate_in_memory();
        let victim_key = register(
            &reg,
            &victim,
            NodeRegistryType::Executor,
            vec![NodeRegistryType::Executor],
        );
        let body = b"vote-payload";
        let sig = signed_body(&signer, body);

        // Signed by `signer`, presented under the registered `victim` key:
        // the envelope must not verify, so the victim is never credited.
        let err = authenticate_envelope(&victim.ed25519, &reg, &sig, &victim_key, body)
            .expect_err("forgery must fail");
        assert!(matches!(err, EnvelopeAuthError::Signature { .. }));
    }

    #[test]
    fn valid_envelope_from_an_unregistered_key_is_unregistered() {
        let reg = registry_with(&[(NodeRegistryType::Executor, 5)]);
        let stranger = NodeIdentity::generate_in_memory();
        let key = stranger.ed25519.public_key().unwrap();
        let body = b"vote-payload";
        let sig = signed_body(&stranger, body);

        let err = authenticate_envelope(&stranger.ed25519, &reg, &sig, &key, body)
            .expect_err("unregistered must fail");
        assert_eq!(
            err,
            EnvelopeAuthError::Unregistered { public_key: key }
        );
    }
}
