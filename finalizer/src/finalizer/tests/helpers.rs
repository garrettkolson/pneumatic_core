//! Shared fixtures for the finalizer test suite (committer convention:
//! all cross-file fixtures live here, pub-ified, and re-export the module
//! header's test-only types).

use super::super::*;
pub use dashmap::DashMap;
pub use pneumatic_core::config::Config;
pub use pneumatic_core::crypto::{AsymCryptoProvider, BasicHashProvider};
pub use pneumatic_core::data::StubDataProvider;
pub use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
pub use pneumatic_core::blocks::{Block, BlockFactory, FinalityStatus};
pub use pneumatic_core::node::{NodeRegistryType, NodeType};
pub use pneumatic_core::tokens::Token;
pub use pneumatic_core::transactions::{
    PendingTransaction, SignedTransaction, TransactionState, TransactionValidationResult,
};
pub use pneumatic_core::rns::identity::NodeIdentity;
pub use pneumatic_core::validation::{
    ShieldedValidationDeps, ShieldedValidationSpec, TransactionValidationSpec,
};

use rand::RngCore;

pub fn make_test_env_data() -> Arc<DashMap<String, EnvironmentMetadata>> {
    let env_map = DashMap::new();
    // Every field `EnvironmentMetadataSpec` requires (the sentinel's
    // fixture is the reference shape); the legacy node-config keys
    // (`security_level`, `data_provider`, …) are unknown to the spec and
    // ignored by serde — kept for diff readability.
    let spec_json = r#"{
        "environment_id": "test_env",
        "environment_name": "test_env",
        "partitions": [
            {"id": "token", "partition_type": "Token"},
            {"id": "slush", "partition_type": "Slush"},
            {"id": "reconciliation", "partition_type": "Other"}
        ],
        "asym_crypto_provider": {"Ed25519": null},
        "sym_crypto_provider": "AES",
        "serialization_provider": "MsgPack",
        "quorum_percentage": 67,
        "override_quorum_percentage": 0.0,
        "security_level": 2,
        "chain_count": 2,
        "node_registry_type": 0,
        "max_stake": 0,
        "min_stake": 0,
        "crypto_provider": "BasicHashProvider",
        "blockchain_metadata": [],
        "block_validators": [],
        "data_provider": "DefaultDataProvider",
        "rest_api_version": 1,
        "is_full_node": true,
        "is_light_node": false,
        "max_in_flight": 100,
        "max_gas_limit": 1000000,
        "max_risk": 1.0,
        "allowed_token_types": [],
        "trans_validation_specs": [],
        "block_validation_specs": [],
        "log_file": "test.log",
        "logger": "FileLogger"
    }"#;
    let spec =
        serde_json::from_str::<EnvironmentMetadataSpec>(spec_json).expect("valid test env JSON");
    let env = EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec");
    env_map.insert(env.environment_id.clone(), env);
    Arc::new(env_map)
}

/// The single `test_env` environment, as the `Arc` the finalizer holds.
pub fn test_env_data() -> Arc<EnvironmentMetadata> {
    Arc::new(
        make_test_env_data()
            .get("test_env")
            .map(|e| e.value().clone())
            .expect("test_env is present in the test env map"),
    )
}

/// A stub `Shielded` spec for S5.2 unit tests: structurally the real
/// lookup seam (registered under `ShieldedValidationSpec::NAME`, the
/// exact name the handler resolves), cheap (no Halo2 keygen) and
/// fail-closed on an empty proof — the same shape the sentinel's S5.1
/// stub uses. The real spec's four checks are pinned in core.
pub struct StubShieldedSpec;

pub fn stub_zero_risk() -> pneumatic_core::errors::TransactionRiskFactor {
    pneumatic_core::errors::TransactionRiskFactor {
        affected_parties: 0,
        amount: 0,
        is_contract: false,
        is_multi_party: false,
    }
}

impl TransactionValidationSpec for StubShieldedSpec {
    fn validate(
        &self,
        _tx: &Transaction,
        _token: &pneumatic_core::tokens::Token,
        _env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        Ok(TransactionValidationResult::valid(vec![], stub_zero_risk()))
    }

    fn calculate_risk(&self, _tx: &Transaction) -> pneumatic_core::errors::TransactionRiskFactor {
        stub_zero_risk()
    }

    fn name(&self) -> &str {
        ShieldedValidationSpec::NAME
    }

    fn validate_shielded(
        &self,
        tx: &ShieldedTransaction,
        _env_data: &EnvironmentMetadata,
        _deps: &ShieldedValidationDeps,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        if tx.proof.is_empty() {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::InvalidShieldedProof,
            ]));
        }
        Ok(TransactionValidationResult::valid(vec![], stub_zero_risk()))
    }
}

/// Test env with the stub spec registered under the real name constant
/// (the lookup seam the handler uses).
pub fn env_with_stub_shielded_spec() -> Arc<EnvironmentMetadata> {
    let mut env = test_env_data().as_ref().clone();
    let mut registry = pneumatic_core::validation::ValidationSpecRegistry::new();
    registry.register_defaults();
    registry.register(Box::new(StubShieldedSpec));
    env.transaction_validation_specs = Arc::new(registry);
    Arc::new(env)
}

/// A pool view whose root history accepts `referenced_root` (fresh at the
/// tip) — the minimal view state for a happy-path S5.2 test.
pub fn pool_view_with_root(referenced_root: [u8; 32]) -> Arc<dyn ShieldedPoolView> {
    let nullifiers = Arc::new(pneumatic_core::registry::NullifierRegistry::new());
    let mut roots = Arc::new(pneumatic_core::shielded::MerkleRootState::new(10));
    Arc::get_mut(&mut roots)
        .expect("unique owner before the view is built")
        .push(referenced_root);
    Arc::new(pneumatic_core::shielded::SimpleShieldedPoolView::with(
        nullifiers, roots,
    ))
}

/// A bare `ShieldedTransaction` fixture (no real note math — the stub spec
/// only inspects `proof`, and the canonical-bytes/hash binding is exactly
/// what S5.2 exercises). `merkle_root` is the view's tip root so check 3
/// would accept it if the real spec ran.
pub fn make_shielded_tx_fixture(id: &str, merkle_root: [u8; 32]) -> ShieldedTransaction {
    ShieldedTransaction {
        id: id.to_string(),
        action: "ShieldedTransfer".to_string(),
        token_id: vec![1, 2, 3],
        spent_commitments: vec![[1u8; 32]],
        nullifiers: vec![[2u8; 32]],
        commitments: vec![[3u8; 32]],
        merkle_root,
        proof: vec![9u8; 64],
        note_ciphertexts: vec![vec![7u8; 64]],
        fee: 0,
    }
}

/// Register `voter` as a `Finalizer` in `registry` so the shielded auth
/// gate recognizes it (S5.2: both shielded arms authenticate the sender as
/// a registered Finalizer — the Executor stage is skipped for shielded
/// transfers).
pub fn register_finalizer(registry: &Arc<NodeRegistry>, voter: &NodeIdentity) {
    registry
        .get_config()
        .type_configs
        .insert(
            NodeRegistryType::Finalizer,
            pneumatic_core::node::NodeTypeConfig { min: 0, max: 10, min_stake: 0 },
        );
    let pk = voter.ed25519.public_key().expect("voter public key");
    assert!(
        registry.register_peer(pk, voter.rhash, &NodeRegistryType::Finalizer, Box::new(NoOpConnection)),
        "finalizer should register within capacity"
    );
}

pub fn make_test_config() -> Config {
    let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let public_key = identity.ed25519.public_key().unwrap_or_default();
    let rhash = identity.rhash;
    Config {
        public_key,
        ip_address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        rest_api_version: 1,
        node_type: NodeType::Full,
        node_registry_types: vec![NodeRegistryType::Finalizer],
        main_environment_id: "test_env".to_string(),
        reconciliation_partition_id: "reconciliation".to_string(),
        environment_metadata: make_test_env_data(),
        type_configs: Arc::new(DashMap::new()),
        identity: Arc::new(identity),
        rhash,
        bootstrap_peers: Vec::new(),
        rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
        transport_enabled: false,
    }
}

pub fn make_test_node_registry() -> Arc<NodeRegistry> {
    let config = make_test_config();
    Arc::new(NodeRegistry::init(
        Arc::new(config),
        None,
        Arc::new(|_, _| true),
    ))
}

pub fn make_test_pending_registry() -> Arc<PendingTransactionRegistry> {
    let registry = Arc::new(PendingTransactionRegistry::new());
    let tx = Transaction {
        id: "test_tx_001".to_string(),
        action: "Transfer".to_string(),
        token_id: vec![0, 1, 2],
        bid: None,
        sequence_number: 1,
        sender: vec![10, 20, 30],
        receiver: vec![40, 50, 60],
        amount: Some(100),
        timestamp: 1000,
        result_hash: vec![1, 2, 3, 4],
        sender_signature: vec![],
    };
    let validation = TransactionValidationResult::valid(
        vec![5, 6, 7, 8], // finalizer key
        pneumatic_core::errors::TransactionRiskFactor {
            affected_parties: 2,
            amount: 100,
            is_contract: false,
            is_multi_party: false,
        },
    );
    let pending = PendingTransaction::new("test_tx_001".to_string(), TransactionState::Validated {
        transaction: tx,
        validation,
    });
    let _ = registry.add_transaction("test_tx_001".to_string(), pending);
    registry
}

pub fn make_test_signing_key() -> (SigningKey, VerifyingKey) {
    let mut seed = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = signing_key.verifying_key();
    (signing_key, verifying_key)
}

pub fn make_finalizer(
    pending_registry: Arc<PendingTransactionRegistry>,
) -> Finalizer {
    make_finalizer_with_registry_and_data_provider(
        make_test_node_registry(),
        pending_registry,
        Arc::new(StubDataProvider::new()),
    )
}

/// Factory that wires a DataProvider with a pre-seeded stake snapshot
/// for the given epoch, so stake fetching tests can verify the
/// `BlockFinalized` gossip path.
pub fn make_finalizer_with_data_provider(
    pending_registry: Arc<PendingTransactionRegistry>,
    data_provider: Arc<StubDataProvider>,
) -> Finalizer {
    make_finalizer_with_registry_and_data_provider(
        make_test_node_registry(),
        pending_registry,
        data_provider,
    )
}

/// Build a finalizer wired to a caller-supplied `node_registry` and
/// `data_provider`. Used by the auth-gate regression tests, which need to
/// register executor voters in the registry before constructing the
/// finalizer.
pub fn make_finalizer_with_registry_and_data_provider(
    node_registry: Arc<NodeRegistry>,
    pending_registry: Arc<PendingTransactionRegistry>,
    data_provider: Arc<StubDataProvider>,
) -> Finalizer {
    // S5.2 defaults: a pristine placeholder pool view (fail-closed against
    // anything non-genesis) and the plain test environment (no `Shielded`
    // spec registered). The shielded tests below use the variants that take
    // an explicit view + spec-registered env.
    make_finalizer_with_shielded(
        node_registry,
        pending_registry,
        data_provider,
        Arc::new(pneumatic_core::shielded::SimpleShieldedPoolView::new(10)),
        test_env_data(),
    )
}

/// Build a finalizer with an explicit shielded pool view + environment.
/// Used by the S5.2 shielded-signing tests, which need a view whose root
/// history accepts the fixture tx's referenced root and an env with a
/// `Shielded` spec registered (a stub for unit-test speed — the real spec's
/// crypto is pinned in the core crate).
pub fn make_finalizer_with_shielded(
    node_registry: Arc<NodeRegistry>,
    pending_registry: Arc<PendingTransactionRegistry>,
    data_provider: Arc<StubDataProvider>,
    pool_view: Arc<dyn ShieldedPoolView>,
    env_data: Arc<EnvironmentMetadata>,
) -> Finalizer {
    make_finalizer_with_shielded_identity(
        node_registry,
        pending_registry,
        data_provider,
        pool_view,
        env_data,
        Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory()),
    )
}

/// Like `make_finalizer_with_shielded`, but with a caller-supplied node
/// identity — the S5.2 quorum-path fixture needs the *same* identity to
/// be (a) the finalizer's own signing identity (its vote's stake is looked
/// up under this key) and (b) a Finalizer peer registered in the node
/// registry (the vote arm's C1 gate).
pub fn make_finalizer_with_shielded_identity(
    node_registry: Arc<NodeRegistry>,
    pending_registry: Arc<PendingTransactionRegistry>,
    data_provider: Arc<StubDataProvider>,
    pool_view: Arc<dyn ShieldedPoolView>,
    env_data: Arc<EnvironmentMetadata>,
    identity: Arc<NodeIdentity>,
) -> Finalizer {
    let signature_registry = Arc::new(TransactionSignatureRegistry::new());
    let hash_provider = Arc::new(BasicHashProvider::new());

    let (signing_key, verifying_key) = make_test_signing_key();

    Finalizer::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        identity,
        node_registry,
        pending_registry,
        signature_registry,
        67.0,   // quorum
        3,      // total voters
        signing_key,
        verifying_key,
        hash_provider,
        vec![10, 20, 30], // leader address
        100,              // leader stake
        vec![40, 50, 60], // leader hash
        0,                // current_epoch
        data_provider,
        "test_env".to_string(),
        pool_view,
        env_data,
    )
}

/// Register `voter` as an `Executor` in `registry` so the finalizer's auth
/// gate (`find_node_type_by_public_key`) recognizes it. The connection is a
/// no-op — the auth gate only needs the key to be present in the Executor
/// shard. Executor capacity must be configured; an unconfigured type has
/// `get_max_node_number == 0` and `register_peer` rejects every peer.
pub fn register_executor(registry: &Arc<NodeRegistry>, voter: &NodeIdentity) {
    registry
        .get_config()
        .type_configs
        .insert(
            NodeRegistryType::Executor,
            pneumatic_core::node::NodeTypeConfig { min: 0, max: 10, min_stake: 0 },
        );
    let pk = voter.ed25519.public_key().expect("voter public key");
    assert!(
        registry.register_peer(pk, voter.rhash, &NodeRegistryType::Executor, Box::new(NoOpConnection)),
        "executor should register within capacity"
    );
}

/// A `Connection` that discards sent data. Only needed because
/// `NodeRegistry::register_peer` requires a `Box<dyn Connection>`; the
/// finalizer's auth gate never sends over it.
pub struct NoOpConnection;

#[async_trait::async_trait]
impl pneumatic_core::conns::Connection for NoOpConnection {
    async fn send(&self, _data: &Vec<u8>) -> Result<(), pneumatic_core::conns::ConnError> {
        Ok(())
    }
}

/// A `Connection` that records every sent payload (the S5.2 full-path
/// fixture uses one per peer to capture the fanned-out `ShieldedVote` and
/// the `Commit` / `BlockFinalized` dispatches).
pub struct RecordingConnection {
    pub recorder: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl pneumatic_core::conns::Connection for RecordingConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), pneumatic_core::conns::ConnError> {
        self.recorder.lock().unwrap().push(data.clone());
        Ok(())
    }
}

/// Pull the first captured wire payload that deserializes to a `Message`
/// with the given action.
pub fn captured_message(
    recorder: &std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    action: &str,
) -> Message {
    let raw = recorder
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .find(|raw| {
            matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == action)
        })
        .unwrap_or_else(|| panic!("no {action} message captured"));
    deserialize_rmp_to(&raw).expect("captured payload should be a Message")
}

/// True when no captured payload is a `Message` with the given action —
/// the fail-closed assertions of the S5.2 discriminator tests.
pub fn captured_message_absent(
    recorder: &std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    action: &str,
) -> bool {
    !recorder
        .lock()
        .unwrap()
        .iter()
        .any(|raw| matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == action))
}

/// Build an authenticated `Sign` message from `voter` for `tx_id` over
/// `transaction_hash`. Both the envelope signature and the inner
/// `TransactionSignature.signature` are real Ed25519 signatures under
/// `voter`'s key, so the finalizer's auth gate accepts the message.
pub fn build_signed_sign_message(
    chain_id: &str,
    tx_id: &[u8],
    transaction_hash: Vec<u8>,
    current_stake: u64,
    voter: &NodeIdentity,
) -> Message {
    let inner_signature = voter
        .ed25519
        .sign_data(&transaction_hash)
        .expect("voter signs transaction hash");
    let sig = TransactionSignature {
        transaction_id: tx_id.to_vec(),
        env_id: chain_id.as_bytes().to_vec(),
        transaction_hash,
        signature: inner_signature,
        current_stake,
    };
    let body = serialize_to_bytes_rmp(&sig).expect("serialize signature");
    Message::signed(chain_id.to_string(), "Sign", body, None, voter)
        .expect("sign envelope")
}

#[test]
pub fn test_finalizer_creation() {
    let pending_registry = make_test_pending_registry();
    let finalizer = make_finalizer(pending_registry);

    assert_eq!(finalizer.env_id, "test_env");
    assert_eq!(finalizer.public_key, vec![1, 2, 3, 4]);
}

pub fn make_stake_set(stakes: Vec<(Vec<u8>, u64)>) -> StakeSet {
    StakeSet {
        stakers: stakes.into_iter().collect(),
    }
}
