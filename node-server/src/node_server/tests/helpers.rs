//! Shared fixtures for the node-server test suite (committer convention:
//! all cross-file fixtures live here, pub-ified, and re-export the module
//! header's test-only types).

pub use std::collections::HashMap;
pub use std::sync::Arc;

pub use dashmap::DashMap;
pub use strum::IntoEnumIterator;

pub use ed25519_dalek::{SigningKey, VerifyingKey};

pub use pneumatic_core::blocks::{BlockFactory, FinalityStatus};
pub use pneumatic_core::config::{BootstrapPeer, Config};
pub use pneumatic_core::conns::Connection;
pub use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
pub use pneumatic_core::errors::PneumaticError;
pub use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
pub use pneumatic_core::messages::Message;
pub use pneumatic_core::node::{NodeTypeConfig, NodeRegistryType};
pub use pneumatic_core::rns::identity::NodeIdentity;
pub use pneumatic_core::registry::TransactionSignatureRegistry;
pub use pneumatic_core::tokens::Token;
pub use pneumatic_core::transactions::{ShieldedTransaction, SignedTransaction, TransactionCommit};
pub use pneumatic_core::user::User;
pub use pneumatic_core::validation::ValidationSpecRegistry;

// S5.4 fixtures (shared by the live composite tests).
pub use pneumatic_core::data::{DataError, ShieldedPoolState};
pub use pneumatic_core::epoch::StakeSet;

pub use pneumatic_core::crypto::{BasicHashProvider, HashProvider};
pub use pneumatic_core::data::{DataProvider, DefaultDataProvider};
pub use pneumatic_core::epoch::{BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector};
pub use pneumatic_core::logging::Logger;
pub use pneumatic_core::node::registry::NodeRegistry;
pub use pneumatic_core::node::stake_index::StakeIndex;
pub use pneumatic_core::registry::PendingTransactionRegistry;
pub use pneumatic_committer::block_services::BlockServices;
pub use pneumatic_committer::epoch_manager::{
    EpochReconciler, LeaderSelector, StakeStore, StakingManager,
};

pub use crate::role_dispatcher::RoleError;
pub use crate::role_selector::StakeProvider;
#[allow(unused_imports)]
use super::super::{
    build_runtime, build_role_plugin, route_data_plane, RoleDispatcher, RoleHandler,
    RoleHost,
};

/// In-memory `DataProvider` for runtime tests (S5.3): models a REACHABLE
/// data service with no records — `get_shielded_pool` returns `Ok(None)`
/// (the pool's boot load then seeds a pristine genesis) and every save is a
/// no-op. All other reads answer "not found" in-process, so these tests
/// never open a socket: production keeps `DefaultDataProvider` and its
/// strict fail-closed boot contract.
#[derive(Default)]
pub struct MemoryDataProvider;

impl DataProvider for MemoryDataProvider {
    fn get_token(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<pneumatic_core::tokens::Token, pneumatic_core::data::DataError> {
        Err(pneumatic_core::data::DataError::DataNotFound)
    }
    fn get_data(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Vec<u8>, pneumatic_core::data::DataError> {
        Err(pneumatic_core::data::DataError::DataNotFound)
    }
    fn get_user(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<pneumatic_core::user::User, pneumatic_core::data::DataError> {
        Err(pneumatic_core::data::DataError::DataNotFound)
    }
    fn get_stake_snapshot(&self, _epoch: u64, _partition_id: &str) -> Result<pneumatic_core::epoch::StakeSet, pneumatic_core::data::DataError> {
        Err(pneumatic_core::data::DataError::StoreNotFound)
    }
    fn save_stake_snapshot(&self, _epoch: u64, _snapshot: pneumatic_core::epoch::StakeSet, _partition_id: &str) -> Result<(), pneumatic_core::data::DataError> {
        Ok(())
    }
    fn get_executor_set(&self, _epoch: u64, _partition_id: &str) -> Result<pneumatic_core::epoch::ExecutorSet, pneumatic_core::data::DataError> {
        Err(pneumatic_core::data::DataError::StoreNotFound)
    }
    fn save_executor_set(&self, _epoch: u64, _set: pneumatic_core::epoch::ExecutorSet, _partition_id: &str) -> Result<(), pneumatic_core::data::DataError> {
        Ok(())
    }
    fn get_shielded_pool(&self, _partition_id: &str) -> Result<Option<pneumatic_core::data::ShieldedPoolState>, pneumatic_core::data::DataError> {
        Ok(None)
    }
    fn save_shielded_pool(&self, _state: &pneumatic_core::data::ShieldedPoolState, _partition_id: &str) -> Result<(), pneumatic_core::data::DataError> {
        Ok(())
    }
}

/// The data provider every `build_runtime` test injects.
pub fn test_data_provider() -> Arc<dyn DataProvider> {
    Arc::new(MemoryDataProvider)
}

/// A complete, valid `EnvironmentMetadataSpec` — the canonical fixture used
/// by the committer/sentinel integration tests, with `environment_id` set to
/// `test_env` (matching `main_environment_id`). Every field the struct
/// requires is present (its validate() also accepts it) so the spec both
/// parses and loads into an `EnvironmentMetadata` with a populated
/// `token_partition_id`, `asym_crypto_provider`, `cost_model`, and `logger`.
pub const SPEC: &str = r#"{
    "environment_id": "test_env",
    "environment_name": "Test Environment",
    "partitions": [
        {"id": "token", "partition_type": "Token"},
        {"id": "reconciliation", "partition_type": "Slush"}
    ],
    "asym_crypto_provider": "Ed25519",
    "sym_crypto_provider": "AES-256-GCM",
    "serialization_provider": "rmp-serde",
    "quorum_percentage": 67.0,
    "override_quorum_percentage": 67.0,
    "max_risk": 1.0,
    "allowed_token_types": [],
    "trans_validation_specs": [],
    "block_validation_specs": [],
    "log_file": "/tmp/test.log",
    "shard_count": 1,
    "shard_quorum_percentage": 67.0
}"#;

/// The in-memory `StakeProvider` the selection path consults — fail-closed
/// (0 on any miss), decoupled from the concrete `StakeIndex` map.
pub struct MapStakeProvider {
    pub values: HashMap<u64, u64>,
    pub default: u64,
}

impl MapStakeProvider {
    pub fn with_default(default: u64) -> Self {
        Self { values: HashMap::new(), default }
    }
}

impl StakeProvider for MapStakeProvider {
    fn stake(&self, _public_key: &[u8], epoch: u64) -> u64 {
        self.values.get(&epoch).copied().unwrap_or(self.default)
    }
}

/// The node's environment registry, with `test_env` loaded from the spec.
pub fn env_registry() -> Arc<DashMap<String, EnvironmentMetadata>> {
    let map = Arc::new(DashMap::new());
    let spec = serde_json::from_str::<EnvironmentMetadataSpec>(SPEC)
        .expect("valid test environment spec");
    let env = EnvironmentMetadata::load_from_spec(spec).expect("valid spec");
    map.insert(env.environment_id.clone(), env);
    map
}

/// Per-type floor: every role requires `floor` stake.
pub fn type_config_floor(floor: u64) -> Arc<DashMap<NodeRegistryType, NodeTypeConfig>> {
    let cfgs = Arc::new(DashMap::new());
    for t in NodeRegistryType::iter() {
        cfgs.insert(t, NodeTypeConfig { min: 1, max: 1000, min_stake: floor });
    }
    cfgs
}

/// Per-type floors such that only `role` qualifies: that role's floor is 0,
/// the others sit far above any stake. `select()` then yields exactly `role`.
pub fn type_config_select(role: NodeRegistryType) -> Arc<DashMap<NodeRegistryType, NodeTypeConfig>> {
    let cfgs = Arc::new(DashMap::new());
    for t in NodeRegistryType::iter() {
        let min_stake = if t == role { 0 } else { 1_000_000 };
        cfgs.insert(t, NodeTypeConfig { min: 1, max: 1000, min_stake });
    }
    cfgs
}

/// A `Config` whose environment is `test_env`, `type_configs` is `configs`,
/// and `bootstrap_peers` is `bootstrap` (a bad public key makes transport
/// fail fast — keeps the host construction hermetic, no RNS binding).
pub fn runtime_config(
    bootstrap: Vec<BootstrapPeer>,
    type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
) -> Arc<Config> {
    let mut cfg = Config::new_for_testing("test_env".into(), env_registry(), type_configs);
    cfg.bootstrap_peers = bootstrap;
    Arc::new(cfg)
}

/// A bad bootstrap public key (`hex::decode` fails in `RnsNetwork::start`)
/// so the transport fails fast and the host boots without it — hermetic,
/// no UDP binding across the shared workspace test runner.
pub fn bad_peer() -> BootstrapPeer {
    BootstrapPeer {
        public_key: "not-a-valid-hex-key".to_string(),
        ip: "127.0.0.1".to_string(),
        port: 0,
    }
}

/// Canonical shape of a routing outcome, so two `Result<(), RoleError>`s
/// (which do not derive `PartialEq`) can be compared for equality.
pub fn outcome_tag(r: Result<(), RoleError>) -> &'static str {
    match r {
        Ok(()) => "ok",
        Err(RoleError::UnknownAction(_)) => "unknown_action",
        Err(RoleError::AmbiguousAction { .. }) => "ambiguous_action",
        Err(RoleError::Downstream(_)) => "downstream",
    }
}

pub fn msg(action: &str) -> Message {
    Message {
        chain_id: "env".into(),
        action: action.to_string(),
        body: vec![],
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    }
}

// -----------------------------------------------------------------------
// S5.4 tests
// -----------------------------------------------------------------------

/// A `Connection` that records each sent payload verbatim (S5.4 e2e
/// relay: the test re-dispatches what each role recorded on its peers).
pub struct RecordingConnection {
    pub recorder: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl Connection for RecordingConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), pneumatic_core::conns::ConnError> {
        self.recorder.lock().unwrap().push(data.clone());
        Ok(())
    }
}

/// In-memory `DataProvider` for the S5.4 live composite tests: the seeded
/// pool state (the pool's boot load; saves update the copy), the opt-in
/// self-verified token (the sentinel's token gates + the finalizer's
/// `resolve_previous_hash`), the submitter user, and the epoch-1 stake
/// snapshot (the finalizer's quorum math + the committer's conflict
/// stakes all read from it — the same lazy snapshot path production uses).
pub struct E2eDataProvider {
    pub pool_state: std::sync::Mutex<ShieldedPoolState>,
    pub token: Token,
    pub user_pk: Vec<u8>,
    pub stakers: std::collections::HashMap<Vec<u8>, u64>,
}

impl DataProvider for E2eDataProvider {
    fn get_token(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Token, DataError> {
        Ok(self.token.clone())
    }
    fn get_data(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Vec<u8>, DataError> {
        Err(DataError::DataNotFound)
    }
    fn get_user(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<User, DataError> {
        Ok(User {
            public_key: self.user_pk.clone(),
            fuel_balance: 10_000,
            stake: 0,
            nonce: 0,
        })
    }
    fn get_stake_snapshot(
        &self,
        _epoch: u64,
        _partition_id: &str,
    ) -> Result<StakeSet, DataError> {
        Ok(StakeSet { stakers: self.stakers.clone() })
    }
    fn save_stake_snapshot(
        &self,
        _epoch: u64,
        _snapshot: StakeSet,
        _partition_id: &str,
    ) -> Result<(), DataError> {
        Ok(())
    }
    fn get_executor_set(
        &self,
        _epoch: u64,
        _partition_id: &str,
    ) -> Result<pneumatic_core::epoch::ExecutorSet, DataError> {
        Err(DataError::StoreNotFound)
    }
    fn save_executor_set(
        &self,
        _epoch: u64,
        _set: pneumatic_core::epoch::ExecutorSet,
        _partition_id: &str,
    ) -> Result<(), DataError> {
        Ok(())
    }
    fn get_shielded_pool(
        &self,
        _partition_id: &str,
    ) -> Result<Option<ShieldedPoolState>, DataError> {
        Ok(Some(self.pool_state.lock().unwrap().clone()))
    }
    fn save_shielded_pool(
        &self,
        state: &ShieldedPoolState,
        _partition_id: &str,
    ) -> Result<(), DataError> {
        *self.pool_state.lock().unwrap() = state.clone();
        Ok(())
    }
}

/// A `Config` whose `test_env` carries the REAL shielded spec (defaults +
/// `register_shielded`) in its transaction-spec registry: the composite
/// sentinel and finalizer advisory gates look the spec up by name, and the
/// minimal `SPEC` above leaves `trans_validation_specs` empty (that would
/// fail closed `UnsupportedAction` before any transfer work). The
/// committer's commit-time re-check is registry-independent (it builds
/// `ShieldedValidationSpec::new()` directly), so only these two arms need
/// the injection.
pub fn runtime_config_with_shielded_spec(
    bootstrap: Vec<BootstrapPeer>,
    type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
) -> Arc<Config> {
    let registry = env_registry();
    {
        let mut ref_mut = registry.get_mut("test_env").expect("test env present");
        let env = ref_mut.value_mut();
        let mut specs = ValidationSpecRegistry::new();
        specs.register_defaults();
        specs.register_shielded();
        env.transaction_validation_specs = Arc::new(specs);
    }
    let mut cfg = Config::new_for_testing("test_env".into(), registry, type_configs);
    cfg.bootstrap_peers = bootstrap;
    Arc::new(cfg)
}

/// A test-build finalizer plugin: the REAL `Finalizer` (the composite
/// finalizer arm's construction, over the shared live pool) whose
/// `allowed_actions` reports `FINALIZER_ACTIONS` with `"SignShielded"`
/// REMOVED. The `handle` body is a loud failure — the dispatcher must
/// never reach it for the removed action.
pub struct ReducedFinalizerPlugin(pub pneumatic_finalizer::Finalizer);

impl RoleHandler for ReducedFinalizerPlugin {
    fn role(&self) -> NodeRegistryType {
        NodeRegistryType::Finalizer
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        &["Sign", "Finalize", "ShieldedVote"]
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            Err(RoleError::Downstream(PneumaticError::Network(format!(
                "reduced finalizer: handler called for {action:?} outside its reduced set",
                action = message.action
            ))))
        })
    }
}

impl RoleHost for ReducedFinalizerPlugin {
    fn advance_epoch(&mut self, _epoch: u64) {
        pneumatic_finalizer::Finalizer::advance_epoch(&mut self.0);
    }
    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            pneumatic_finalizer::Finalizer::initiate_shutdown(&self.0).await
        })
    }
}

// --- e2e test fixtures --------------------------------------------------

/// The shielded token the e2e universe targets: self-verified + opt-in.
pub fn make_e2e_token() -> Token {
    let mut t = Token::new();
    t.id = vec![1];
    t.is_self_verified = true;
    t.set_metadata("shielded_opt_in".into(), "true".into());
    t
}

pub fn e2e_seed_delta(block_hash: Vec<u8>, leaves: Vec<[u8; 32]>, post_root: [u8; 32])
-> pneumatic_core::data::AppliedPoolDelta {
    pneumatic_core::data::AppliedPoolDelta {
        block_hash,
        leaves,
        nullifiers: vec![],
        post_root,
    }
}

/// The wire block for an e2e shielded commit: the canonical plain block
/// shape (the committer's H12 tx-hash pairing is against the EMBEDDED
/// plain transaction, byte-identical to what the finalizer embeds),
/// carrying the shielded payload, chained off `prev_hash`.
pub fn make_e2e_shielded_block(
    stx: &pneumatic_core::transactions::ShieldedTransaction,
    prev_hash: Vec<u8>,
) -> pneumatic_core::blocks::Block {
    let signed = SignedTransaction {
        shielded: Some(stx.clone()),
        transaction_id: stx.id.clone(),
        transaction: pneumatic_core::transactions::Transaction {
            id: stx.id.clone(),
            action: "ShieldedTransfer".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b"alice".to_vec(),
            receiver: vec![2],
            amount: None,
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        },
        total_voters: 3,
        total_stake: 42,
        leader_hash: prev_hash.clone(),
        leader_address: vec![],
        leader_stake: 0,
        finalizer_addr: vec![],
        finalizer_sig: pneumatic_core::transactions::TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![],
            current_stake: 0,
        },
        executor_sigs: std::collections::HashMap::new(),
        proposer_key: vec![],
    };
    let mut block = pneumatic_core::blocks::Block {
        signed_trans: signed,
        token_metadata: std::collections::HashMap::new(),
        previous_hash: prev_hash,
        timestamp: 0,
        current_hash: vec![],
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash =
        BlockFactory::create_hash(&block).expect("well-formed test block hashes");
    block
}

pub fn e2e_commit(
    stx: &pneumatic_core::transactions::ShieldedTransaction,
    block: &pneumatic_core::blocks::Block,
) -> TransactionCommit {
    TransactionCommit {
        trans_id: stx.id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test_env".to_string(),
        proposed_block: block.clone(),
    }
}

/// The signed wire `Commit` message (envelope: the proposer's finalizer
/// identity — registered as a Finalizer, which `Commit` permits).
pub fn e2e_commit_message(
    commit: &TransactionCommit,
    identity: &NodeIdentity,
) -> Message {
    let body = serialize_to_bytes_rmp(commit).expect("commit serializes");
    Message::signed("env".to_string(), "Commit", body, None, identity).expect("signs")
}

/// Poll the recorder until a message with `action` appears (the
/// fire-and-forget gossiper sends on a worker thread), then return it.
pub async fn next_recorded(
    recorder: &Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    action: &str,
) -> Message {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (found, n) = {
            let guard = recorder.lock().unwrap();
            let found = guard
                .iter()
                .find(|raw| {
                    deserialize_rmp_to::<Message>(raw)
                        .map(|m| m.action == action)
                        .unwrap_or(false)
                })
                .cloned();
            (found, guard.len())
        };
        if let Some(raw) = found {
            return deserialize_rmp_to::<Message>(&raw).expect("recorded payload is a Message");
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for a recorded {action:?} (have {n} payloads)");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The pool's live tree root, rebuilt from the snapshot's leaf set (for
/// the root-history coherence assertion) — `current_root()` is the
/// history tip; the tree root over the same leaves must equal it.
pub fn pool_tree_root(
    pool: &pneumatic_committer::shielded_pool::ShieldedPool,
) -> [u8; 32] {
    use pneumatic_core::shielded::{
        bytes_to_root, IncrementalMerkleTree, DEFAULT_DEPTH,
    };
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    for leaf in pool.state_snapshot().leaves {
        tree.append_leaf(&bytes_to_root(&leaf).expect("snapshot leaf is a committed leaf"));
    }
    pneumatic_core::shielded::root_to_bytes(&tree.root())
}
