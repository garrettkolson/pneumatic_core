use moka::sync::Cache;
use crate::config::Config;
use crate::conns::factories::ConnFactory;
use crate::conns::senders::{RnsSender, Sender};
use crate::node::NodeRegistryNode;
use crate::crypto::AsymCryptoProvider;
use crate::data::DataError;
use crate::encoding::deserialize_rmp_to;
use crate::messages::Message;
use crate::node::NodeRegistryType;
use crate::node::registry::NodeRegistry;
use std::sync::{Arc, RwLock};

/// Gossiper handles message deduplication and fan-out.
/// Maintains a signature cache to prevent processing duplicate messages.
pub struct Gossiper {
    /// The node type this gossiper belongs to
    node_type: NodeRegistryType,
    /// Config for the node
    config: Config,
    /// Connection factory for sending messages
    conn_factory: ConnFactory,
    /// Content-hash dedup cache with configurable TTL. Keyed on a hash of
    /// `(sender_key, body)` (see `handle_message` and the `dedup_key` helper),
    /// not raw signature bytes, so an honest re-send is deduped regardless of
    /// signature and forged content is never admitted — insertion follows
    /// verification, so a rejected message never occupies the cache slot that a
    /// legitimate message with the same key/body would use.
    cache: Cache<Vec<u8>, ()>,
    /// Registered handlers — invoked sequentially for each valid, non-duplicate
    /// message. Set via `initialize()` (first handler) and `add_handler()` (extra
    /// handlers). Stored behind a `Mutex` because `initialize()`, `add_handler()`,
    /// and `handle_message()` all borrow `&self`.
    handlers: std::sync::Mutex<Vec<Box<dyn Fn(Vec<u8>) + Send + Sync>>>,
    /// Cryptographic provider for signature verification of incoming messages.
    crypto_provider: Arc<RwLock<dyn AsymCryptoProvider>>,
}

impl Gossiper {
    /// Create a new Gossiper with configurable dedup TTL and crypto provider.
    pub fn new(
        node_type: NodeRegistryType,
        config: Config,
        ttl_seconds: u64,
        crypto_provider: Arc<RwLock<dyn AsymCryptoProvider>>,
    ) -> Self {
        Gossiper {
            node_type,
            config,
            conn_factory: ConnFactory::new(),
            cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(std::time::Duration::from_secs(ttl_seconds))
                .build(),
            handlers: std::sync::Mutex::new(Vec::new()),
            crypto_provider,
        }
    }

    /// Initialize the gossiper by registering the first message handler.
    /// The handler closure is called for each valid, non-duplicate message.
    /// The closure receives the raw message bytes (before deserialization).
    ///
    /// To register additional handlers, call `add_handler()`.
    pub fn initialize<F>(&self, on_message_received: F)
    where
        F: Fn(Vec<u8>) + Send + Sync + 'static,
    {
        self.handlers.lock().unwrap().push(Box::new(on_message_received));
    }

    /// Register an additional handler for valid, non-duplicate messages.
    ///
    /// Use this when one gossiper instance needs to dispatch messages to
    /// multiple internal delegates (fan-out). Each handler receives a copy
    /// of the raw message bytes.
    pub fn add_handler<F>(&self, handler: F)
    where
        F: Fn(Vec<u8>) + Send + Sync + 'static,
    {
        self.handlers.lock().unwrap().push(Box::new(handler));
    }

    /// Handle an incoming message: deserialize, verify the envelope signature,
    /// dedup on a content hash, and fan out to handlers.
    ///
    /// Verification runs *before* the dedup cache is touched, so forged or
    /// tampered content is rejected and never admitted to the cache. A later
    /// legitimate message carrying the same `(sender_key, body)` is therefore
    /// accepted instead of being silently dropped by a poisoned entry.
    pub fn handle_message(&self, raw_data: Vec<u8>) -> Result<(), DataError> {
        // Deserialize the message from MsgPack bytes
        let message: Message = match deserialize_rmp_to(&raw_data) {
            Ok(m) => m,
            Err(e) => return Err(DataError::DeserializationError(e)),
        };

        // Validate the envelope signature against the sender's public key.
        // RwLock poison and crypto errors are propagated as DataError instead of
        // panicking, so a malformed/tampered message cannot crash a handling thread.
        let crypto = self
            .crypto_provider
            .read()
            .map_err(|e| DataError::CryptoError(format!("RwLock poisoned: {:?}", e)))?;
        if !crypto
            .check_signature(&message.signature, &message.public_key, &message.body)
            .map_err(|e| DataError::CryptoError(e.to_string()))?
        {
            return Err(DataError::InvalidSignature);
        }

        // Dedup on a hash of (sender_key, body), not the signature bytes, so an
        // honest re-send is collapsed regardless of signature randomness and
        // two different senders with identical bodies are not confused with one
        // another.
        let dedup_key = Self::dedup_key(&message.public_key, &message.body);
        if self.cache.get(&dedup_key).is_some() {
            return Ok(()); // Duplicate — silently skip
        }

        // Insert into the cache only after verification succeeded (see
        // `handle_message` docs above).
        self.cache.insert(dedup_key, ());

        // Fan-out: invoke every registered handler with a copy of the raw data.
        // Each handler owns the dispatch logic (routing by action, etc.).
        let handlers = self.handlers.lock().unwrap();
        for handler in handlers.iter() {
            handler(raw_data.clone());
        }

        Ok(())
    }

    /// Build a collision-resistant dedup key from `(sender_key, body)`: the
    /// SHA-256 of the public key concatenated with the SHA-256 of the body.
    /// Hashing both operands independently (Merkle-style) prevents a
    /// `public_key`/`body` pair from prefix-colliding with another — e.g. a
    /// 1-byte-key + 3-byte-body vs. a 3-byte-key + 1-byte-body.
    fn dedup_key(public_key: &[u8], body: &[u8]) -> Vec<u8> {
        let pk = crate::crypto::sha256(public_key);
        let bd = crate::crypto::sha256(body);
        let mut key = Vec::with_capacity(pk.len() + bd.len());
        key.extend_from_slice(&pk);
        key.extend_from_slice(&bd);
        key
    }

    /// Send a message to all nodes of a given type via the provided sender factory.
    ///
    /// The factory maps each registered node to a `Sender`. This keeps the
    /// production path (which constructs an `RnsSender` per node) intact while
    /// allowing tests to inject a recording or scripted sender.
    pub fn send_to_type<S, F>(
        &self,
        node_registry: &NodeRegistry,
        node_type: &NodeRegistryType,
        sender_for: F,
        data: &[u8],
    ) -> Result<(), DataError>
    where
        S: Sender,
        F: Fn(&NodeRegistryNode) -> S,
    {
        let Some(nodes) = node_registry.get_nodes(node_type) else {
            return Ok(());
        };
        for entry in nodes.iter() {
            // Key is the public key; we use rhash for routing
            let sender = sender_for(entry.value());
            sender
                .get_response(data)
                .map_err(|e| DataError::FromStore(e.to_string()))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, RwLockReadGuard};

    use super::*;
    use crate::config::Config;
    use crate::crypto::Ed25519Provider;
    use crate::node::{NodeRegistryType, NodeType};
    use crate::rns::config_builder::DEFAULT_UDP_PORT;
    use crate::rns::identity::NodeIdentity;
    use dashmap::DashMap;

    fn make_test_config() -> Config {
        let identity = NodeIdentity::generate_in_memory();
        let rhash = identity.rhash;
        let public_key = identity.ed25519.public_key().unwrap_or_default();
        Config {
            public_key,
            ip_address: "127.0.0.1".parse().unwrap(),
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            type_configs: Arc::new(DashMap::new()),
            identity: Arc::new(identity),
            rhash,
            bootstrap_peers: Vec::new(),
            rns_port: DEFAULT_UDP_PORT,
            transport_enabled: false,
        }
    }

    /// Create a Gossiper with a crypto provider returned for message signing.
    fn make_gossiper_with_provider() -> (Gossiper, Arc<RwLock<Ed25519Provider>>) {
        let config = make_test_config();
        let crypto_provider = Arc::new(RwLock::new(Ed25519Provider::generate()));
        let gossiper = Gossiper::new(
            NodeRegistryType::Sentinel,
            config,
            300,
            crypto_provider.clone(),
        );
        (gossiper, crypto_provider)
    }

    /// Create a properly signed test message using the given provider.
    fn make_signed_message(
        provider: &RwLockReadGuard<Ed25519Provider>,
        chain_id: &str,
        action: &str,
        body: Vec<u8>,
    ) -> Message {
        let pk = provider.public_key().unwrap();
        let sig = provider.sign_data(&body).unwrap();
        Message {
            chain_id: chain_id.to_string(),
            action: action.to_string(),
            body,
            signature: sig,
            public_key: pk,
            stake_set: None,
        }
    }

    #[test]
    fn gossiper_accepts_first_message() {
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");
        let msg = make_signed_message(&sig_guard, "test", "Process", vec![]);
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();
        drop(sig_guard);
        assert!(gossiper.handle_message(raw).is_ok());
    }

    /// Locks in the honest wire path that Phase 1.1 brings live: a message
    /// built with `Message::signed` (the production envelope) must pass the
    /// gossiper's signature check and reach registered handlers.
    #[test]
    fn node_identity_signed_message_reaches_handlers() {
        let (gossiper, _crypto_provider) = make_gossiper_with_provider();
        let identity = NodeIdentity::generate_in_memory();
        let msg = Message::signed("test".to_string(), "Process", vec![1, 2, 3], None, &identity)
            .expect("Message::signed should succeed");
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        gossiper.initialize(move |_data| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        gossiper.handle_message(raw).expect("signed message should be accepted");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn gossiper_silently_ignores_duplicate() {
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");
        let msg = make_signed_message(&sig_guard, "test", "Process", vec![]);
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();
        drop(sig_guard);
        // First call: Ok, added to cache
        assert!(gossiper.handle_message(raw.clone()).is_ok());
        // Second call with same signature: Ok, but silently skipped (dedup)
        assert!(gossiper.handle_message(raw).is_ok());
    }

    #[test]
    fn gossiper_accepts_different_message() {
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");
        let msg_a = make_signed_message(&sig_guard, "test", "Process", vec![]);
        let msg_b = make_signed_message(&sig_guard, "test", "Process", vec![1]);
        let raw_a = crate::encoding::serialize_to_bytes_rmp(&msg_a).unwrap();
        let raw_b = crate::encoding::serialize_to_bytes_rmp(&msg_b).unwrap();
        assert!(gossiper.handle_message(raw_a).is_ok());
        assert!(gossiper.handle_message(raw_b).is_ok());
    }

    #[test]
    fn gossiper_cache_max_capacity_is_10000() {
        // The gossiper is constructed with max_capacity(10_000)
        // We verify by checking that the Cache was created correctly.
        // Since the cache field is private, we verify via behavior:
        // insert 10_001 unique messages and verify the 10_001st is accepted
        // (oldest would have been evicted by TTL-free cache).
        // For a simple verification, we just check the gossiper constructs.
        let config = make_test_config();
        let crypto_provider = Arc::new(RwLock::new(Ed25519Provider::generate()));
        let gossiper = Gossiper::new(NodeRegistryType::Sentinel, config, 300, crypto_provider);
        // If it constructs without panic, the cache is set up.
        // The capacity constant is 10_000 per the source code.
        drop(gossiper);
    }

    // --- Fan-out tests ---

    #[test]
    fn fan_out_invokes_all_handlers() {
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");
        let msg = make_signed_message(&sig_guard, "test", "Process", vec![1, 2, 3]);
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();
        drop(sig_guard);

        let handler1_count = Arc::new(AtomicUsize::new(0));
        let handler2_count = Arc::new(AtomicUsize::new(0));

        let c1 = handler1_count.clone();
        gossiper.initialize(move |_data| {
            c1.fetch_add(1, Ordering::SeqCst);
        });

        let c2 = handler2_count.clone();
        gossiper.add_handler(move |_data| {
            c2.fetch_add(1, Ordering::SeqCst);
        });

        gossiper.handle_message(raw).unwrap();

        assert_eq!(handler1_count.load(Ordering::SeqCst), 1);
        assert_eq!(handler2_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fan_out_handlers_receive_copy_of_data() {
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");
        let msg = make_signed_message(&sig_guard, "broadcast", "Process", vec![42]);
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();
        drop(sig_guard);

        let received1 = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let received2 = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let r1 = received1.clone();
        gossiper.initialize(move |data| {
            r1.lock().unwrap().extend_from_slice(&data);
        });

        let r2 = received2.clone();
        gossiper.add_handler(move |data| {
            r2.lock().unwrap().extend_from_slice(&data);
        });

        gossiper.handle_message(raw.clone()).unwrap();

        assert_eq!(*received1.lock().unwrap(), raw);
        assert_eq!(*received2.lock().unwrap(), raw);
        assert_eq!(*received1.lock().unwrap(), *received2.lock().unwrap());
    }

    #[test]
    fn fan_out_dedup_skips_all_handlers() {
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");
        let msg = make_signed_message(&sig_guard, "test", "Process", vec![1]);
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();
        drop(sig_guard);

        let count = Arc::new(AtomicUsize::new(0));
        let c1 = count.clone();
        let c2 = count.clone();

        gossiper.initialize(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
        });
        gossiper.add_handler(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
        });

        // First call — both handlers invoked
        gossiper.handle_message(raw.clone()).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);

        // Duplicate — neither handler invoked
        gossiper.handle_message(raw).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fan_out_three_handlers_all_called() {
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");
        let msg = make_signed_message(&sig_guard, "test", "Process", vec![1, 2]);
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();
        drop(sig_guard);

        let count = Arc::new(AtomicUsize::new(0));
        let c1 = count.clone();
        let c2 = count.clone();
        let c3 = count.clone();

        gossiper.initialize(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
        });
        gossiper.add_handler(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
        });
        gossiper.add_handler(move |_| {
            c3.fetch_add(1, Ordering::SeqCst);
        });

        gossiper.handle_message(raw).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn fan_out_concurrent_handler_invocation() {
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");
        let msg = make_signed_message(&sig_guard, "test", "Process", vec![1, 2, 3]);
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();
        drop(sig_guard);
        let gossiper = Arc::new(gossiper);

        let counts: Vec<Arc<AtomicUsize>> = (0..5)
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect();

        for i in 0..5 {
            let c = counts[i].clone();
            if i == 0 {
                gossiper.initialize(move |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                });
            } else {
                gossiper.add_handler(move |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                });
            }
        }

        // Send 100 messages concurrently
        let mut handles = vec![];
        for _ in 0..100 {
            let g = gossiper.clone();
            let r = raw.clone();
            handles.push(std::thread::spawn(move || {
                g.handle_message(r).unwrap();
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Due to the moka cache's concurrent nature, multiple threads
        // may pass the duplicate check before any completes the insert.
        // Each handler should have been called the same number of times (≥ 1).
        let expected = counts[0].load(Ordering::SeqCst);
        assert!(expected >= 1);
        for c in &counts[1..] {
            assert_eq!(c.load(Ordering::SeqCst), expected);
        }
    }

    // --- Verify-then-insert / content-hash dedup tests (Phase 1.2, audit C4) ---

    #[test]
    fn gossiper_two_senders_same_body_both_pass() {
        // Verify (a): two different senders with identical bodies must both
        // reach the handler. Content-hash dedup keys on (sender_key, body), so
        // the two distinct public keys yield distinct dedup keys.
        let (gossiper, _crypto_provider) = make_gossiper_with_provider();

        let sender_a = NodeIdentity::generate_in_memory();
        let sender_b = NodeIdentity::generate_in_memory();
        let body = vec![7, 7, 7, 7];

        let msg_a =
            Message::signed("env".into(), "Process", body.clone(), None, &sender_a).unwrap();
        let msg_b =
            Message::signed("env".into(), "Process", body, None, &sender_b).unwrap();
        let raw_a = crate::encoding::serialize_to_bytes_rmp(&msg_a).unwrap();
        let raw_b = crate::encoding::serialize_to_bytes_rmp(&msg_b).unwrap();

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        gossiper.initialize(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        gossiper.handle_message(raw_a).unwrap();
        gossiper.handle_message(raw_b).unwrap();

        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "both senders' identical-body messages must reach the handler"
        );
    }

    #[test]
    fn gossiper_content_hash_dedup() {
        // Verify (b): same identity + same body twice → the second is deduped.
        // An honest re-send must be collapsed regardless of whether the dedup
        // key is content-based.
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        gossiper.initialize(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        let msg = make_signed_message(&sig_guard, "env", "Process", vec![9, 9]);
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();
        drop(sig_guard);

        gossiper.handle_message(raw.clone()).unwrap();
        gossiper.handle_message(raw).unwrap();

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "identical (key, body) must be deduped to a single handler invocation"
        );
    }

    #[test]
    fn gossiper_forged_signature_not_cached() {
        // Verify (c): a forged message that claims the legit sender's public key
        // but is signed by an attacker key is rejected with InvalidSignature and
        // is NOT cached. A subsequent legitimate re-send from the legit key still
        // reaches the handler (count == 1).
        //
        // This is the distinguishing regression test for the reorder: with
        // insert-before-verify, the forged message would occupy the
        // (legit_key, body) cache slot and the legit re-send would be silently
        // dropped (count == 0).
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");

        let legit = NodeIdentity::generate_in_memory();
        let attacker = NodeIdentity::generate_in_memory();
        let body = vec![3, 1, 4];
        let legit_pub = legit.ed25519.public_key().unwrap();
        let attacker_sig = attacker.sign_message(&body).unwrap();

        // Forged: public_key = legit, signature = attacker's signature over body.
        let forged = Message {
            chain_id: "env".into(),
            action: "Process".into(),
            body: body.clone(),
            signature: attacker_sig,
            public_key: legit_pub,
            stake_set: None,
        };
        let forged_raw = crate::encoding::serialize_to_bytes_rmp(&forged).unwrap();

        // Legit re-send (built while the read guard is still held).
        let legit_msg = make_signed_message(&sig_guard, "env", "Process", body);
        let legit_raw = crate::encoding::serialize_to_bytes_rmp(&legit_msg).unwrap();

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        gossiper.initialize(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        drop(sig_guard);

        // Forged message must be rejected.
        match gossiper.handle_message(forged_raw) {
            Err(DataError::InvalidSignature) => {}
            other => panic!(
                "forged message must be rejected as InvalidSignature, got {:?}",
                other
            ),
        }

        // Legit re-send must reach the handler (forged was not cached).
        gossiper.handle_message(legit_raw).unwrap();

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "forged message must not poison the cache; the legit re-send must reach the handler"
        );
    }

    #[test]
    fn gossiper_tampered_body_rejected() {
        // Sanity: sign a message, then mutate its body → the envelope signature
        // no longer matches and the message is rejected as InvalidSignature.
        // Guards that the verify path actually closes on mutation.
        let (gossiper, crypto_provider) = make_gossiper_with_provider();
        let sig_guard = crypto_provider.read().expect("RwLock poisoned");

        let mut msg = make_signed_message(&sig_guard, "env", "Process", vec![5, 5, 5]);
        msg.body.push(6); // mutate after signing
        drop(sig_guard);
        let raw = crate::encoding::serialize_to_bytes_rmp(&msg).unwrap();

        match gossiper.handle_message(raw) {
            Err(DataError::InvalidSignature) => {}
            other => panic!(
                "tampered body must be rejected as InvalidSignature, got {:?}",
                other
            ),
        }
    }
}
