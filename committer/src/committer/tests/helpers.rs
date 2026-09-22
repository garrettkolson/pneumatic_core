// Shared test infrastructure: the in-memory DataProvider mock, CollectingLogger,
// RecordingConnection, and every builder/attribute-less helper used across the
// per-domain test modules. Types are re-exported here and pulled in by domain
// test modules with `use super::helpers::*;`.

// Re-export all shared types (used both inside helpers.rs and by domain modules).
pub use crate::committer_error::CommitterError;
pub use dashmap::DashMap;
pub use pneumatic_core::blocks::Block;
pub use pneumatic_core::config::Config;
pub use pneumatic_core::crypto::{AsymCryptoProvider, BasicHashProvider, Ed25519Provider};
pub use pneumatic_core::data::{DataError, DataProvider, StubDataProvider};
pub use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
pub use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
pub use pneumatic_core::epoch::{BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector, ExecutorSet};
pub use pneumatic_core::errors::TransactionRiskFactor;
pub use pneumatic_core::gossiper::Gossiper;
pub use pneumatic_core::messages::Message;
pub use pneumatic_core::node::registry::NodeRegistry;
pub use pneumatic_core::node::NodeRegistryType;
pub use pneumatic_core::registry::PendingTransactionRegistry;
pub use pneumatic_core::transactions::{PendingTransaction, SignedTransaction, Transaction, TransactionCommit, TransactionSignature, TransactionState, TransactionValidationResult};
pub use pneumatic_core::user::User;
pub use std::collections::HashMap;

use super::super::*;
use std::sync::{Arc, Mutex};


// --- In-memory DataProvider mock for tests ---

pub struct TestDataProvider {
    pub users: Mutex<HashMap<Vec<u8>, HashMap<String, User>>>,
    /// When true, `get_user` returns an error (simulates a data-service failure).
    pub fail_get: bool,
    /// When true, `save_user` returns an error (simulates a data-service failure).
    pub fail_save: bool,
    /// When true, `save_stake_snapshot`/`save_executor_set` return an error
    /// (simulates a snapshot-persistence failure).
    pub fail_snapshot_save: bool,
}


impl TestDataProvider {
    pub fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            fail_get: false,
            fail_save: false,
            fail_snapshot_save: false,
        }
    }

    /// `new()` with both simulated data-service failures armed.
    pub fn with_failures(fail_get: bool, fail_save: bool) -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            fail_get,
            fail_save,
            fail_snapshot_save: false,
        }
    }

    /// Arm the stake/executor snapshot-persistence failure, so `save_stake_snapshot`
    /// and `save_executor_set` return `Err`. Used to prove `advance_epoch_to` surfaces a
    /// persistence error rather than swallowing it (AUDIT Phase 5.4 / M8).
    pub fn with_snapshot_save_failure(mut self, fail: bool) -> Self {
        self.fail_snapshot_save = fail;
        self
    }
    pub fn insert_user(&self, key: Vec<u8>, partition_id: String, user: User) {
        self.users
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .insert(partition_id, user);
    }

    /// Read a user's stored balance directly from the backing map, bypassing the fail toggles.
    /// Lets an assertion confirm a value survived a simulated data-service failure (when the
    /// normal `get_user` path is deliberately returning `Err`).
    pub fn raw_balance(&self, key: &[u8], partition_id: &str) -> Option<u64> {
        self.users
            .lock()
            .unwrap()
            .get(key)
            .and_then(|partitions| partitions.get(partition_id))
            .map(|u| u.fuel_balance)
    }
}


impl DataProvider for TestDataProvider {
    fn get_token(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Token, DataError> {
        Err(DataError::DataNotFound)
    }
    fn save_token(&self, _key: &Vec<u8>, _token: Token, _partition_id: &str) -> Result<(), DataError> {
        Ok(())
    }
    fn save_data(&self, _key: &Vec<u8>, _data: Vec<u8>, _partition_id: &str) -> Result<(), DataError> {
        Ok(())
    }
    fn get_user(&self, key: &Vec<u8>, partition_id: &str) -> Result<User, DataError> {
        if self.fail_get {
            return Err(DataError::StoreNotFound);
        }
        self.users
            .lock()
            .unwrap()
            .get(key)
            .and_then(|partitions| partitions.get(partition_id))
            .cloned()
            .ok_or(DataError::DataNotFound)
    }
    fn save_user(&self, key: &Vec<u8>, user: User, partition_id: &str) -> Result<(), DataError> {
        if self.fail_save {
            return Err(DataError::StoreNotFound);
        }
        self.users
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .insert(partition_id.to_string(), user);
        Ok(())
    }

    fn get_stake_snapshot(&self, _epoch: u64, _partition_id: &str) -> Result<StakeSet, DataError> {
        Ok(StakeSet::default())
    }

    fn save_stake_snapshot(&self, _epoch: u64, _snapshot: StakeSet, _partition_id: &str) -> Result<(), DataError> {
        if self.fail_snapshot_save {
            return Err(DataError::StoreNotFound);
        }
        Ok(())
    }

    fn get_executor_set(&self, _epoch: u64, _partition_id: &str) -> Result<ExecutorSet, DataError> {
        Ok(ExecutorSet::default())
    }

    fn save_executor_set(&self, _epoch: u64, _set: ExecutorSet, _partition_id: &str) -> Result<(), DataError> {
        if self.fail_snapshot_save {
            return Err(DataError::StoreNotFound);
        }
        Ok(())
    }
}


pub fn make_test_env_data() -> EnvironmentMetadata {
    let json = r#"{"environment_id":"test","environment_name":"test",
        "partitions":[{"id":"token","partition_type":"Token"},
        {"id":"slush","partition_type":"Slush"}],
        "asym_crypto_provider":{"Ed25519":null},"sym_crypto_provider":"sym",
        "serialization_provider":"rmp","quorum_percentage":67.0,
        "override_quorum_percentage":0.0,"max_risk":1.0,
        "allowed_token_types":[],"trans_validation_specs":[],
        "block_validation_specs":[],"log_file":"test.log"}"#;
    let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).unwrap();
    EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec")
}


/// The canonical test `Transaction` shared by every block/entry builder in this module. Built
/// in one place so the committed block (`make_test_block_for_token` / `make_block_with_proposer`)
/// and the pending registry entry (`make_finalizing_entry` / `make_validated_entry`) carry a
/// byte-identical payload. This is now required: the Committer rejects a commit whose block
/// embeds a transaction that differs from the validated one (AUDIT Phase 3.5 / H12), so a
/// committed block and its registry entry must hash equal.
pub fn make_test_transaction(tx_id: &str, sender: Vec<u8>) -> Transaction {
    Transaction {
        id: tx_id.to_string(),
        action: "Process".into(),
        token_id: vec![1],
        bid: None,
        sequence_number: 1,
        sender,
        receiver: vec![2],
        amount: Some(100),
        timestamp: 0,
        result_hash: vec![],
        sender_signature: vec![],
    }
}


/// Create a block that chains off the token's current chain state.
pub fn make_test_block_for_token(
    committer: &Committer,
    trans_id: &str,
    sender: Vec<u8>,
) -> Block {
    make_block_for_token_id(committer, trans_id, sender, &vec![1])
}


/// Generic variant of `make_test_block_for_token` that reads the chain state of an
/// arbitrary `token_id`, so tests can build a block for a second, independent token
/// (each chaining off its own genesis).
pub fn make_block_for_token_id(
    committer: &Committer,
    trans_id: &str,
    sender: Vec<u8>,
    token_id: &[u8],
) -> Block {
    // Get the chain's last hash (empty previous_hash at genesis)
    let prev_hash = if let Some(entry) = committer.tokens.get(token_id) {
        let token = entry.value();
        let state = token.blockchain.get_current_chain_state();
        if state.last_hash_in.is_empty() {
            // Genesis convention: block 1 has an empty previous_hash
            Vec::<u8>::new()
        } else {
            state.last_hash_in
        }
    } else {
        Vec::<u8>::new()
    };

    let signed = SignedTransaction {
        shielded: None,
        transaction_id: trans_id.to_string(),
        transaction: make_test_transaction(trans_id, sender),
        total_voters: 3,
        total_stake: 42,
        leader_hash: prev_hash.clone(),
        leader_address: vec![],
        leader_stake: 0,
        finalizer_addr: vec![],
        finalizer_sig: TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![],
            current_stake: 0,
        },
        executor_sigs: HashMap::new(),
        proposer_key: vec![],
    };

    let mut block = Block {
        signed_trans: signed,
        token_metadata: HashMap::new(),
        previous_hash: prev_hash,
        timestamp: 0,
        current_hash: vec![],
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block)
        .expect("well-formed test block hashes");
    block
}


pub fn bootstrap_token_chain(committer: &Committer) {
    // Pre-seed a genesis block so these tests exercise the
    // non-empty-chain path. Also mark the token self-verified: with
    // fail-closed block validation (AUDIT Phase 3.2 / C5) a process-style
    // block committed on this chain only validates under the "SelfSigned"
    // spec, which gates on token.is_self_verified.
    if let Some(mut entry) = committer.tokens.get_mut(&vec![1]) {
        entry.value_mut().is_self_verified = true;
    }

    let prev_hash = vec![42u8; 32];
    let signed = SignedTransaction::test_transaction();
    let mut genesis = Block {
        signed_trans: signed,
        token_metadata: HashMap::new(),
        previous_hash: prev_hash,
        timestamp: 0,
        current_hash: vec![],
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    genesis.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&genesis)
        .expect("well-formed test block hashes");

    if let Some(mut entry) = committer.tokens.get_mut(&vec![1]) {
        entry.value_mut().blockchain.add_block(genesis);
    }
}


/// Builds a Committer with the default full-stake slash fraction (1.0).
pub fn make_test_committer(
    data_provider: Arc<TestDataProvider>,
) -> (Committer, Arc<PendingTransactionRegistry>, Arc<CollectingLogger>) {
    make_test_committer_with_slash(data_provider, 1.0)
}


/// Builds a Committer with an overridable `CostModel.slash_fraction`. The default
/// `make_test_committer` delegates here with the full-stake default (1.0).
pub fn make_test_committer_with_slash(
    data_provider: Arc<TestDataProvider>,
    slash_fraction: f64,
) -> (Committer, Arc<PendingTransactionRegistry>, Arc<CollectingLogger>) {
    let mut env_data = Arc::new(make_test_env_data());
    // Install an in-memory collecting logger so a test can assert a failure path emitted an
    // observable log line (the default FileLogger discards to a file). `env_data` is uniquely
    // owned right here, so `Arc::get_mut` reaches the metadata to swap the logger without
    // changing EnvironmentMetadata's shape or the public API. The slash fraction is set here too
    // so a test can drive a partial (vs. full) slash of a double-signed proposer's stake.
    let logger = CollectingLogger::default();
    {
        let env = Arc::get_mut(&mut env_data)
            .expect("env_data uniquely owned at construction time");
        env.logger = Arc::new(logger.clone());
        env.cost_model.slash_fraction = slash_fraction;
    }
    let identity = Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory());
    let rhash = identity.rhash;
    let config = Config {
        public_key: vec![1],
        ip_address: "127.0.0.1".parse().unwrap(),
        rest_api_version: 1,
        node_type: pneumatic_core::node::NodeType::Full,
        node_registry_types: vec![NodeRegistryType::Committer],
        main_environment_id: "test".to_string(),
        reconciliation_partition_id: "recon".to_string(),
        environment_metadata: Arc::new(DashMap::new()),
        // Capacity entries are required — without them get_max_node_number
        // returns 0 and register_peer rejects every peer.
        type_configs: Arc::new({
            let tc = DashMap::new();
            tc.insert(NodeRegistryType::Committer.clone(),
                pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 });
            tc.insert(NodeRegistryType::Sentinel.clone(),
                pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 });
            tc.insert(NodeRegistryType::Archiver.clone(),
                pneumatic_core::node::NodeTypeConfig { min: 1, max: 10, min_stake: 0 });
            tc
        }),
        identity: identity.clone(),
        rhash,
        bootstrap_peers: Vec::new(),
        rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
        transport_enabled: false,
    };
    let node_registry = Arc::new(NodeRegistry::init(
        Arc::new(config),
        None,
        Arc::new(|_, _| true),
    ));

    let gossiper_identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let gossiper_rhash = gossiper_identity.rhash;
    let gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Committer,
        Config {
            public_key: vec![2],
            ip_address: "127.0.0.1".parse().unwrap(),
            rest_api_version: 1,
            node_type: pneumatic_core::node::NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            type_configs: Arc::new(DashMap::new()),
            identity: Arc::new(gossiper_identity),
            rhash: gossiper_rhash,
            bootstrap_peers: Vec::new(),
            rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
            transport_enabled: false,
        },
        60,
        env_data.asym_crypto_provider.clone(),
    ));

    let tokens = Arc::new(DashMap::new());
    let pending_registry = Arc::new(PendingTransactionRegistry::new());
    let stake_store = Arc::new(StakeStore::new());
    let staking_manager = Arc::new(StakingManager::new(stake_store.clone(), env_data.logger.clone()));
    let data_provider_core = Arc::new(pneumatic_core::data::DefaultDataProvider::new());
    let candidate_registry = Arc::new(CandidateRegistry::new());
    let epoch_reconciler = Arc::new(EpochReconciler::new(
        stake_store.clone(),
        candidate_registry.clone(),
        data_provider_core.clone(),
        "test".to_string(),
        vec![vec![1]], // token ID from bootstrap_token
        env_data.cost_model.slash_fraction,
    ));
    let hash_provider = Arc::new(BasicHashProvider::new());
    let leader_selector = Arc::new(LeaderSelector::new(hash_provider));

    // Epoch tracking components
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let epoch_duration = 300;
    let initial_epoch = Epoch::new_with_leader(
        1,
        now,
        now + epoch_duration,
        leader_selector.as_ref(),
        &stake_store.to_stake_set(),
        &[], // genesis: no prior block → empty prev_block_hash
    );
    let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
    let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));

    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider_core.clone(),
        node_registry.clone(),
        env_data.clone(),
        env_data.logger.clone(),
        identity.clone(),
    ));

    let committer = Committer::new(
        env_data.clone(),
        vec![1],
        identity,
        gossiper,
        block_services,
        node_registry,
        tokens,
        pending_registry.clone(),
        stake_store,
        staking_manager,
        epoch_reconciler,
        leader_selector,
        data_provider,
        0,
        Some(epoch_detector),
        block_proposer,
        epoch_duration,
        5000,
        candidate_registry,
    );

    (committer, pending_registry, Arc::new(logger))
}


pub fn make_finalizing_entry(
    pending_registry: &PendingTransactionRegistry,
    tx_id: &str,
    sender: Vec<u8>,
) {
    pending_registry.register_pending(tx_id.to_string()).unwrap();
    let tx = make_test_transaction(tx_id, sender.clone());
    // Transition directly via the internal map — register_pending creates Pending,
    // then we mutate to Finalizing state.
    {
        let mut entry = pending_registry.get_transaction_mut(tx_id).unwrap();
        entry.transition_to_validated(tx.clone(),
            pneumatic_core::transactions::TransactionValidationResult {
                is_valid: true,
                risk: pneumatic_core::errors::TransactionRiskFactor {
                    affected_parties: 2, amount: 100,
                    is_contract: false, is_multi_party: false,
                },
                failure_reasons: vec![],
                finalizer_public_key: vec![3],
            });
        entry.transition_to_finalizing(tx, vec![3]);
    }
}


// --- AUDIT Phase 4.5 / M11: a failed gas deduction is surfaced and observable, and the
//     per-sender read-modify-write is serialized so concurrent commits cannot lose an update ---

/// In-memory Logger for tests: captures every `log` call so a test can assert that a
/// failure path produced an observable line (the real FileLogger discards to a file).
#[derive(Default, Clone)]
pub struct CollectingLogger {
    pub logs: Arc<Mutex<Vec<String>>>,
}


impl Logger for CollectingLogger {
    fn log(&self, message: String) {
        self.logs.lock().unwrap().push(message);
    }
}


/// Assert `result` is a `GasDeduction` error carrying the expected sender / tx_id / gas.
pub fn assert_gas_deduction(
    result: Result<(), CommitterError>,
    sender: &[u8],
    tx_id: &str,
    gas_used: u64,
) {
    match result {
        Err(CommitterError::GasDeduction {
            sender: got_sender,
            tx_id: got_tx_id,
            gas_used: got_gas,
            ..
        }) => {
            assert_eq!(got_sender, bytes_to_hex(sender));
            assert_eq!(got_tx_id, tx_id);
            assert_eq!(got_gas, gas_used);
        }
        other => panic!("expected CommitterError::GasDeduction, got {other:?}"),
    }
}


pub fn make_validated_entry(
    pending_registry: &PendingTransactionRegistry,
    tx_id: &str,
    sender: Vec<u8>,
) {
    pending_registry.register_pending(tx_id.to_string()).unwrap();
    let tx = make_test_transaction(tx_id, sender.clone());
    // Transition to Validated (NOT Finalizing) — simulates leader-proposal path
    {
        let mut entry = pending_registry.get_transaction_mut(tx_id).unwrap();
        entry.transition_to_validated(tx.clone(),
            pneumatic_core::transactions::TransactionValidationResult {
                is_valid: true,
                risk: pneumatic_core::errors::TransactionRiskFactor {
                    affected_parties: 2, amount: 100,
                    is_contract: false, is_multi_party: false,
                },
                failure_reasons: vec![],
                finalizer_public_key: vec![3],
            });
    }
}


// --- propose_blocks and advance_epoch tests ---

/// Build a Committer where the leader is controlled by `leader_key`.
/// The committer's own public key is `committer_key`.
pub fn make_committer_for_leader_test(
    committer_key: Vec<u8>,
    leader_key: Vec<u8>,
) -> (Committer, Arc<PendingTransactionRegistry>, Arc<TestDataProvider>) {
    build_committer_for_leader_test(committer_key, leader_key, Arc::new(TestDataProvider::new()))
}


/// The full committer construction for leader/epoch tests, parameterized on the injected
/// data provider. The public `make_committer_for_leader_test` wraps this with a default
/// provider so existing call sites are unaffected; tests that need a failing provider
/// (e.g. snapshot-persistence failures, AUDIT Phase 5.4 / M8) call this directly.
pub fn build_committer_for_leader_test(
    committer_key: Vec<u8>,
    leader_key: Vec<u8>,
    dp: Arc<TestDataProvider>,
) -> (Committer, Arc<PendingTransactionRegistry>, Arc<TestDataProvider>) {
    let env_data = Arc::new(make_test_env_data());
    let identity = Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory());
    let rhash = identity.rhash;
    let config = Config {
        public_key: committer_key.clone(),
        ip_address: "127.0.0.1".parse().unwrap(),
        rest_api_version: 1,
        node_type: pneumatic_core::node::NodeType::Full,
        node_registry_types: vec![NodeRegistryType::Committer],
        main_environment_id: "test".to_string(),
        reconciliation_partition_id: "recon".to_string(),
        environment_metadata: Arc::new(DashMap::new()),
        type_configs: Arc::new(DashMap::new()),
        identity: identity.clone(),
        rhash,
        bootstrap_peers: Vec::new(),
        rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
        transport_enabled: false,
    };
    let node_registry = Arc::new(NodeRegistry::init(
        Arc::new(config),
        None,
        Arc::new(|_, _| true),
    ));

    let gossiper_identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let gossiper_rhash = gossiper_identity.rhash;
    let gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Committer,
        Config {
            public_key: vec![2],
            ip_address: "127.0.0.1".parse().unwrap(),
            rest_api_version: 1,
            node_type: pneumatic_core::node::NodeType::Full,
            node_registry_types: vec![NodeRegistryType::Committer],
            main_environment_id: "test".to_string(),
            reconciliation_partition_id: "recon".to_string(),
            environment_metadata: Arc::new(DashMap::new()),
            type_configs: Arc::new(DashMap::new()),
            identity: Arc::new(gossiper_identity),
            rhash: gossiper_rhash,
            bootstrap_peers: Vec::new(),
            rns_port: pneumatic_core::rns::config_builder::DEFAULT_UDP_PORT,
            transport_enabled: false,
        },
        60,
        env_data.asym_crypto_provider.clone(),
    ));

    let tokens = Arc::new(DashMap::new());
    let pending_registry = Arc::new(PendingTransactionRegistry::new());
    let stake_store = Arc::new(StakeStore::new());
    let staking_manager = Arc::new(StakingManager::new(stake_store.clone(), env_data.logger.clone()));
    let data_provider_core = Arc::new(pneumatic_core::data::DefaultDataProvider::new());
    let candidate_registry = Arc::new(CandidateRegistry::new());
    let epoch_reconciler = Arc::new(EpochReconciler::new(
        stake_store.clone(),
        candidate_registry.clone(),
        data_provider_core.clone(),
        "test".to_string(),
        vec![vec![1]],
        env_data.cost_model.slash_fraction,
    ));
    let hash_provider = Arc::new(BasicHashProvider::new());
    let leader_selector = Arc::new(LeaderSelector::new(hash_provider));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let epoch_duration = 3600; // 1 hour — is_epoch_expired returns false
    let initial_epoch = Epoch {
        epoch_number: 1,
        start_timestamp: now,
        end_timestamp: now + epoch_duration,
        leader_public_key: leader_key.clone(),
    };
    let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
    let block_proposer = Arc::new(BlockProposer::new(leader_key, 100, vec![]));

    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider_core.clone(),
        node_registry.clone(),
        env_data.clone(),
        env_data.logger.clone(),
        identity.clone(),
    ));

    let test_dp = dp;
    let committer = Committer::new(
        env_data,
        committer_key,
        identity,
        gossiper,
        block_services,
        node_registry,
        tokens,
        pending_registry.clone(),
        stake_store,
        staking_manager,
        epoch_reconciler,
        leader_selector,
        test_dp.clone(),
        1,
        Some(epoch_detector),
        block_proposer,
        epoch_duration,
        5000,
        candidate_registry,
    );

    (committer, pending_registry, test_dp)
}


// --- Conflict resolution at commit time ---

/// Build a block with a specific proposer_key for conflict testing.
pub fn make_block_with_proposer(
    committer: &Committer,
    trans_id: &str,
    proposer_key: Vec<u8>,
) -> Block {
    // Genesis convention: block 1 has an empty previous_hash
    let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
        let token = entry.value();
        let state = token.blockchain.get_current_chain_state();
        if state.last_hash_in.is_empty() {
            Vec::<u8>::new()
        } else {
            state.last_hash_in
        }
    } else {
        Vec::<u8>::new()
    };

    // The transaction sender is fixed to `alice` here (all commit-test callers register their
    // entry as alice); the only per-call variance is the `proposer_key`, which lives outside the
    // transaction payload. Build via the shared helper so the committed block hashes equal the
    // registry entry (AUDIT Phase 3.5 / H12).
    let signed = SignedTransaction {
        shielded: None,
        transaction_id: trans_id.to_string(),
        transaction: make_test_transaction(trans_id, b"alice".to_vec()),
        total_voters: 3,
        total_stake: 42,
        leader_hash: prev_hash.clone(),
        leader_address: vec![],
        leader_stake: 0,
        finalizer_addr: vec![],
        finalizer_sig: TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![],
            current_stake: 0,
        },
        executor_sigs: HashMap::new(),
        proposer_key: proposer_key.clone(),
    };

    let mut block = Block {
        signed_trans: signed,
        token_metadata: HashMap::new(),
        previous_hash: prev_hash,
        timestamp: 0,
        current_hash: vec![],
        finality_status: FinalityStatus::Optimistic,
        proposer_key,
        epoch_number: 0,
    };
    block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block)
        .expect("well-formed test block hashes");
    block
}


// -----------------------------------------------------------------------
// Block gossip tests
// -----------------------------------------------------------------------

/// Build a valid block for the given transaction, chained off the current tip.
/// Build a gossip block that chains off the token's **live** chain tip, carrying a valid,
/// self-consistent finalizer signature (AUDIT Phase 3.3 / C5). `verify_block_finalizer_sig`
/// re-checks this signature in `handle_block_finalized`, so an empty or forged signature would
/// now be rejected — a pre-fix `make_gossip_block` used `signature: vec![]` and slipped through.
pub fn make_gossip_block(committer: &Committer, trans_id: &str, proposer_key: Vec<u8>) -> Block {
    let prev_hash = if let Some(entry) = committer.tokens.get(&vec![1]) {
        entry.value().blockchain.get_current_chain_state().last_hash_in
    } else {
        vec![42u8; 32]
    };
    make_gossip_block_at_prev(trans_id, proposer_key, &prev_hash)
}


/// Build a gossip block with a caller-supplied `previous_hash` (so several blocks can share a
/// frozen parent, as in a sibling-race test) and a valid finalizer signature. Distinct
/// `trans_id`s yield distinct `transaction_hash`/`signature` values, hence distinct
/// `current_hash` — exactly what a true sibling race needs. The finalizer signs the stored
/// `transaction_hash` (which `create_hash` binds via `CanonicalSignedTransaction`), so the
/// signature and the block hash are mutually consistent.
pub fn make_gossip_block_at_prev(
    trans_id: &str,
    proposer_key: Vec<u8>,
    prev_hash: &[u8],
) -> Block {
    // A throwaway test finalizer key. `finalizer_addr` is its public key; `signature` is an
    // Ed25519 sign over the stored `transaction_hash`, which `verify_block_finalizer_sig`
    // re-checks against `finalizer_addr`. Any fixed value works for `transaction_hash` — it
    // only has to survive inside the canonical bytes that `create_hash` hashes.
    let finalizer = Ed25519Provider::generate();
    let finalizer_addr = finalizer.public_key().expect("finalizer public key");
    let transaction_hash = format!("gossip-{trans_id}").into_bytes();
    let signature = finalizer.sign_data(&transaction_hash).expect("finalizer signature");

    let signed = SignedTransaction {
        shielded: None,
        transaction_id: trans_id.to_string(),
        transaction: Transaction {
            id: trans_id.to_string(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b"alice".to_vec(),
            receiver: b"bob".to_vec(),
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        },
        total_voters: 3,
        total_stake: 42,
        leader_hash: prev_hash.to_vec(),
        leader_address: vec![],
        leader_stake: 0,
        finalizer_addr: finalizer_addr.clone(),
        finalizer_sig: TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: transaction_hash.clone(),
            signature,
            current_stake: 0,
        },
        executor_sigs: HashMap::new(),
        proposer_key: proposer_key,
    };

    let mut block = Block {
        signed_trans: signed,
        token_metadata: HashMap::new(),
        previous_hash: prev_hash.to_vec(),
        current_hash: vec![],
        timestamp: 0,
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash =
        BlockFactory::create_hash(&block).expect("well-formed test block hashes");
    block
}


/// Build a wire Message for a BlockFinalized gossip event.
pub fn make_block_finalized_message(block: Block) -> Message {
    let body = serialize_to_bytes_rmp(&block).expect("Block serialization");
    Message {
        chain_id: "test".to_string(),
        action: String::from("BlockFinalized"),
        body,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    }
}


/// Helper to build a test block with a valid hash.
pub fn make_test_block_for_token_internal(blockchain: &pneumatic_core::blocks::Blockchain) -> Block {
    let prev_hash: Vec<u8> = if blockchain.get_count() == 0 {
        vec![42u8; 32]
    } else {
        blockchain.get_current_chain_state().last_hash_in
    };

    let signed = SignedTransaction {
        shielded: None,
        transaction_id: "test".to_string(),
        transaction: Transaction {
            id: "test".to_string(),
            action: "Process".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: b"alice".to_vec(),
            receiver: b"bob".to_vec(),
            amount: Some(100),
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
        finalizer_sig: TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: vec![],
            signature: vec![],
            current_stake: 0,
        },
        executor_sigs: HashMap::new(),
        proposer_key: vec![],
    };

    let mut block = Block {
        signed_trans: signed,
        token_metadata: HashMap::new(),
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


// -----------------------------------------------------------------------
// Phase 1.1 regression: outbound broadcasts signed with node identity
// -----------------------------------------------------------------------

/// A Connection that records each sent payload verbatim.
pub struct RecordingConnection {
    pub recorder: Arc<Mutex<Vec<Vec<u8>>>>,
}


#[async_trait::async_trait]
impl pneumatic_core::conns::Connection for RecordingConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), pneumatic_core::conns::ConnError> {
        self.recorder.lock().unwrap().push(data.clone());
        Ok(())
    }
}


/// Assert the message envelope is signed by `identity` — the same check
/// the gossiper performs: signature over `body` under `public_key`.
pub fn assert_signed_by(message: &Message, identity: &pneumatic_core::rns::identity::NodeIdentity) {
    let expected_pk = identity.ed25519.public_key().expect("identity pubkey");
    assert_eq!(
        message.public_key, expected_pk,
        "message.public_key must be the sender's identity key"
    );
    let verifier = pneumatic_core::crypto::Ed25519Provider::generate();
    let ok = verifier
        .check_signature(&message.signature, &message.public_key, &message.body)
        .expect("signature check should succeed");
    assert!(ok, "message body must verify under the sender's identity key");
}
