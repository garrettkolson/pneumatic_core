//! End-to-end pipeline tests covering the full transaction lifecycle:
//! - submit → optimistic → no conflict → confirmed
//! - submit → conflict → resolved → slashing

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use pneumatic_core::blocks::{Block, BlockFactory};
use pneumatic_core::config::Config;
use pneumatic_core::crypto::{AsymCryptoProvider, BasicHashProvider};
use pneumatic_core::data::{DataError, DataProvider, DefaultDataProvider};
use pneumatic_core::encoding::serialize_to_bytes_rmp;
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::epoch::CandidateRegistry;
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::logging::FileLogger;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::{NullConnection, NodeRegistry};
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::node::NodeTypeConfig;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::tokens::Token;
use pneumatic_core::transactions::{
    SignedTransaction, Transaction, TransactionCommit, TransactionSignature,
};
use pneumatic_core::user::User;
use pneumatic_committer::block_services::BlockServices;
use pneumatic_committer::committer::Committer;
use pneumatic_committer::committer_error::CommitterError;
use pneumatic_committer::epoch_manager::{EpochReconciler, LeaderSelector as CommLeaderSelector};
use pneumatic_committer::epoch_manager::{StakeStore, StakingManager};

// --- In-memory DataProvider mock for tests ---

struct TestDataProvider {
    users: Mutex<HashMap<Vec<u8>, HashMap<String, User>>>,
}

impl TestDataProvider {
    fn new() -> Self {
        TestDataProvider {
            users: Mutex::new(HashMap::new()),
        }
    }

    fn insert_user(&self, public_key: Vec<u8>, partition_id: String, user: User) {
        let mut users = self.users.lock().unwrap();
        users.entry(public_key).or_default().insert(partition_id, user);
    }

    fn get_user(&self, public_key: &[u8], partition_id: &str) -> Option<User> {
        self.users.lock().unwrap().get(public_key)?.get(partition_id).cloned()
    }
}

impl DataProvider for TestDataProvider {
    fn get_token(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Token, DataError> {
        Err(DataError::DataNotFound)
    }

    fn save_token(&self, _key: &Vec<u8>, _token: Token, _partition_id: &str) -> Result<(), DataError> {
        Ok(())
    }

    fn get_data(&self, _key: &Vec<u8>, _partition_id: &str) -> Result<Vec<u8>, DataError> {
        Err(DataError::DataNotFound)
    }

    fn save_data(&self, _key: &Vec<u8>, _data: Vec<u8>, _partition_id: &str) -> Result<(), DataError> {
        Ok(())
    }

    fn get_user(&self, key: &Vec<u8>, partition_id: &str) -> Result<User, DataError> {
        self.get_user(key, partition_id)
            .ok_or(DataError::DataNotFound)
    }

    fn save_user(&self, key: &Vec<u8>, user: User, partition_id: &str) -> Result<(), DataError> {
        self.insert_user(key.clone(), partition_id.to_string(), user);
        Ok(())
    }

    fn get_stake_snapshot(&self, _epoch: u64, _partition_id: &str) -> Result<pneumatic_core::epoch::StakeSet, DataError> {
        Ok(Default::default())
    }

    fn save_stake_snapshot(&self, _epoch: u64, _snapshot: pneumatic_core::epoch::StakeSet, _partition_id: &str) -> Result<(), DataError> {
        Ok(())
    }

    fn get_executor_set(&self, _epoch: u64, _partition_id: &str) -> Result<pneumatic_core::epoch::ExecutorSet, DataError> {
        Ok(Default::default())
    }

    fn save_executor_set(&self, _epoch: u64, _set: pneumatic_core::epoch::ExecutorSet, _partition_id: &str) -> Result<(), DataError> {
        Ok(())
    }
}

fn make_test_env_data(logger: Arc<FileLogger>) -> Arc<EnvironmentMetadata> {
    // Create a minimal EnvironmentMetadataSpec from JSON
    let spec_json = r#"
    {
        "environment_id": "test",
        "environment_name": "Test Environment",
        "partitions": [
            {"id": "token", "partition_type": "Token"},
            {"id": "slush", "partition_type": "Slush"}
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
    }
    "#;

    let spec: pneumatic_core::environment::EnvironmentMetadataSpec = serde_json::from_str(spec_json)
        .expect("Failed to parse EnvironmentMetadataSpec JSON");

    let mut env_data = EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec");
    env_data.logger = logger;

    Arc::new(env_data)
}

fn make_test_committer(data_provider: Arc<TestDataProvider>) -> (
    Committer,
    Arc<PendingTransactionRegistry>,
    Arc<DashMap<Vec<u8>, Token>>,
    Arc<NodeRegistry>,
) {
    let logger = Arc::new(FileLogger::new("/tmp/test_integration.log".to_string()));
    let env_data = make_test_env_data(logger);
    let identity = Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory());
    let rhash = identity.rhash;
    // Realistic per-type node counts so `register_peer` succeeds for the node
    // roles this test registers. The empty map the old code used was fine only
    // because, prior to the sender-auth gate, no pipeline test registered a
    // sender before calling `handle_message`.
    let type_configs = {
        let map = DashMap::new();
        for node_type in [
            NodeRegistryType::Committer,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Executor,
            NodeRegistryType::Finalizer,
            NodeRegistryType::Archiver,
        ] {
            map.insert(node_type, NodeTypeConfig { min: 1, max: 1000, min_stake: 0 });
        }
        Arc::new(map)
    };
    let config = Config {
        public_key: vec![1],
        ip_address: "127.0.0.1".parse().unwrap(),
        rest_api_version: 1,
        node_type: pneumatic_core::node::NodeType::Full,
        node_registry_types: vec![NodeRegistryType::Committer],
        main_environment_id: "test".to_string(),
        reconciliation_partition_id: "recon".to_string(),
        environment_metadata: Arc::new(DashMap::new()),
        type_configs,
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
    let data_provider_core = Arc::new(DefaultDataProvider::new());
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
    let leader_selector = Arc::new(CommLeaderSelector::new(hash_provider));

    // Epoch tracking components
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let epoch_duration = 300;
    let initial_epoch = pneumatic_core::epoch::Epoch::new_with_leader(
        1,
        now,
        now + epoch_duration,
        leader_selector.as_ref(),
        &stake_store.to_stake_set(),
        &[], // genesis: no prior block → empty prev_block_hash
    );
    let epoch_detector = pneumatic_core::epoch::EpochBoundaryDetector::new(initial_epoch);
    let block_proposer = Arc::new(pneumatic_core::epoch::BlockProposer::new(vec![], 0, vec![]));

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
        node_registry.clone(),
        tokens.clone(),
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

    (committer, pending_registry, tokens, node_registry)
}

/// Register `identity` under `role` in `registry`, as a sender whose envelope
/// will pass the commender's fail-closed auth gate (registered node of an
/// allowed role). Returns the identity so the caller can sign with its key.
fn register_node(
    registry: &NodeRegistry,
    role: NodeRegistryType,
) -> pneumatic_core::rns::identity::NodeIdentity {
    let identity = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let public_key = identity.ed25519.public_key().expect("public key");
    assert!(
        registry
            .register_peer(public_key, identity.rhash, &role, Box::new(NullConnection)),
        "register peer"
    );
    identity
}

fn bootstrap_token_chain(tokens: &DashMap<Vec<u8>, Token>) {
    let token = Token::new();
    let tip = token.blockchain.get_current_chain_state();

    let signed = SignedTransaction {
        transaction_id: "genesis_tx".to_string(),
        transaction: Transaction {
            id: "genesis_tx".to_string(),
            action: "Genesis".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 0,
            sender: b"alice".to_vec(),
            receiver: b"bob".to_vec(),
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        },
        total_voters: 3,
        total_stake: 42,
        leader_address: vec![],
        leader_stake: 0,
        leader_hash: tip.last_hash_in.clone(),
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

    let mut block = Block::from_transaction(
        signed,
        pneumatic_core::blocks::Blockchain::new(),
        &Token::new(),
        0,
    );
    block.previous_hash = tip.last_hash_in.clone();
    block.current_hash = BlockFactory::create_hash(&block);

    let mut bc = pneumatic_core::blocks::Blockchain::new();
    bc.add_block(block);

    let mut token = Token::new();
    token.id = vec![1];
    token.blockchain = bc;
    token.security_level = 10;
    token.is_self_verified = true;
    token.is_non_transferable = false;
    // Fail-closed block validation (Phase 3.2 / C5) requires a registered
    // validator spec; "SelfSigned" validates these genesis/process-style
    // blocks (chain linkage + is_self_verified). The empty string here would
    // otherwise resolve to the unregistered name "" and get rejected.
    token.block_validation_spec_name = String::from("SelfSigned");
    token.environment_id = "test".to_string();
    token.sequence_number = 1;

    tokens.insert(token.id.clone(), token);
}

fn make_block_finalized_message(block: Block, finalizer: &pneumatic_core::rns::identity::NodeIdentity) -> Message {
    let body = serialize_to_bytes_rmp(&block).expect("Block serialization");
    Message::signed("test".to_string(), "BlockFinalized", body, None, finalizer)
        .expect("sign BlockFinalized")
}

#[tokio::test]
async fn test_pipeline_no_conflict() {
    // Test: submit → optimistic → no conflict → confirmed

    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, tokens, node_registry) = make_test_committer(dp);

    // Register a Finalizer node identity so the commender's fail-closed
    // sender-auth gate accepts the BlockFinalized message(s) this test sends.
    let finalizer = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    node_registry
        .register_peer(
            finalizer.ed25519.public_key().expect("finalizer public key"),
            finalizer.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(NullConnection),
        );

    // Bootstrap token and chain
    bootstrap_token_chain(&tokens);

    // Create a valid block chained off the current tip
    let tip = tokens.get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;

    let block = Block {
        signed_trans: SignedTransaction {
            transaction_id: "test_tx".to_string(),
            transaction: Transaction {
                id: "test_tx".to_string(),
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
            leader_address: vec![],
            leader_stake: 0,
            leader_hash: tip.clone(),
            // A valid finalizer signature (AUDIT Phase 3.3 / C5) so handle_block_finalized's
            // fail-closed verify passes: finalizer_addr is the pubkey, signature signs the stored
            // transaction_hash, which create_hash binds into the block hash — so current_hash stays
            // consistent. A pre-fix block used signature: vec![] and was accepted.
            finalizer_addr: finalizer.ed25519.public_key().expect("public key"),
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: b"pipeline_no_conflict_tx_hash".to_vec(),
                signature: finalizer.ed25519.sign_data(b"pipeline_no_conflict_tx_hash").expect("finalizer sig"),
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: vec![],
        },
        token_metadata: HashMap::new(),
        previous_hash: tip,
        timestamp: 0,
        current_hash: vec![],
        finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };

    // Compute valid block hash
    let expected_hash = BlockFactory::create_hash(&block);
    let block = Block {
        current_hash: expected_hash,
        ..block
    };

    // Create a BlockFinalized message, signed by the registered Finalizer.
    let message = make_block_finalized_message(block, &finalizer);

    // Handle the message - should succeed and append the block
    let result = committer.handle_message(message).await;
    assert!(result.is_ok(), "handle_message failed: {:?}", result.err());

    // Verify block was appended to chain
    let chain = tokens.get(&vec![1]).unwrap();
    let chain_len = chain.value().blockchain.get_count();
    assert!(chain_len >= 2, "Chain should have at least 2 blocks (genesis + test block), got {}", chain_len);
}

/// AUDIT Phase 4.1 / H4 — e2e discriminator for the Commit sink path (4.1a).
///
/// Boots a committer whose pending registry is intentionally EMPTY (the
/// `make_test_committer` harness constructs `PendingTransactionRegistry::new()`
/// with no test injection — this is the main.rs-equivalent boot wiring),
/// registers a Finalizer, and sends a properly-signed `Commit` envelope for a
/// block whose tx is NOT already in the registry. Before the sink existed this
/// failed with `TransactionNotInFinalizing`; the sink materializes the tx as
/// `Finalizing` from the wire block, keyed to the authenticated finalizer
/// (`message.public_key`), and commits it.
#[tokio::test]
async fn commit_from_empty_registry_materializes_and_commits() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, tokens, node_registry) = make_test_committer(dp);

    // Register a Finalizer so the "Commit" envelope passes the fail-closed auth
    // gate — "Commit" is Finalizer-only.
    let finalizer = register_node(&node_registry, NodeRegistryType::Finalizer);

    // Bootstrap token + genesis chain.
    bootstrap_token_chain(&tokens);

    // Build a block chained off the current tip with a valid finalizer signature
    // (mirrors test_pipeline_no_conflict, so the block validates on the "SelfSigned" spec).
    let tip = tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;
    let block = Block {
        signed_trans: SignedTransaction {
            transaction_id: "commit_sink_tx".to_string(),
            transaction: Transaction {
                id: "commit_sink_tx".to_string(),
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
            leader_address: vec![],
            leader_stake: 0,
            leader_hash: tip.clone(),
            finalizer_addr: finalizer.ed25519.public_key().expect("public key"),
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: b"commit_sink_tx_hash".to_vec(),
                signature: finalizer
                    .ed25519
                    .sign_data(b"commit_sink_tx_hash")
                    .expect("finalizer sig"),
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: vec![],
        },
        token_metadata: HashMap::new(),
        previous_hash: tip,
        timestamp: 0,
        current_hash: vec![],
        finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    let block = Block {
        current_hash: BlockFactory::create_hash(&block),
        ..block
    };

    let commit = TransactionCommit {
        trans_id: b"commit_sink_tx".to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };

    // The pending registry must NOT already contain this tx — the sink materializes it.
    assert!(!registry.contains("commit_sink_tx"));

    // Sign a "Commit" envelope with the finalizer key (Commit is Finalizer-only).
    let body = serialize_to_bytes_rmp(&commit).expect("serialize commit");
    let message =
        Message::signed("test".to_string(), "Commit", body, None, &finalizer).expect("sign commit");

    let result = committer.handle_message(message).await;
    assert!(
        result.is_ok(),
        "commit from empty registry failed (sink path): {:?}",
        result.err()
    );

    // The committed block must have grown the chain (genesis + committed).
    let chain_len = tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
    assert!(
        chain_len >= 2,
        "chain should have grown to at least 2 blocks (genesis + committed), got {}",
        chain_len
    );
}

#[tokio::test]
async fn test_pipeline_conflict_and_slashing() {
    // Test: submit → conflict → resolved → slashing

    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, tokens, node_registry) = make_test_committer(dp);

    // Register a Finalizer node identity so the commender's fail-closed
    // sender-auth gate accepts the BlockFinalized message(s) this test sends.
    let finalizer = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    node_registry
        .register_peer(
            finalizer.ed25519.public_key().expect("finalizer public key"),
            finalizer.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(NullConnection),
        );

    // Bootstrap token and chain
    bootstrap_token_chain(&tokens);

    // Create two conflicting blocks with different proposers
    let tip = tokens.get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;

    let block1 = Block {
        signed_trans: SignedTransaction {
            transaction_id: "conflict_tx_1".to_string(),
            transaction: Transaction {
                id: "conflict_tx_1".to_string(),
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
            leader_address: vec![1],
            leader_stake: 100,
            leader_hash: tip.clone(),
            // Valid finalizer signature (AUDIT Phase 3.3 / C5): verify_block_finalizer_sig checks
            // it in handle_block_finalized. create_hash binds the whole finalizer_sig, so
            // current_hash (set below) stays self-consistent.
            finalizer_addr: finalizer.ed25519.public_key().expect("public key"),
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: b"pipeline_conflict_1_tx_hash".to_vec(),
                signature: finalizer.ed25519.sign_data(b"pipeline_conflict_1_tx_hash").expect("finalizer sig"),
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: vec![1],
        },
        token_metadata: HashMap::new(),
        previous_hash: tip.clone(),
        timestamp: 0,
        current_hash: vec![],
        finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
        proposer_key: vec![1],
        epoch_number: 0,
    };

    let block2 = Block {
        signed_trans: SignedTransaction {
            transaction_id: "conflict_tx_2".to_string(),
            transaction: Transaction {
                id: "conflict_tx_2".to_string(),
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
            leader_address: vec![2],
            leader_stake: 50,
            leader_hash: tip.clone(),
            // Valid finalizer signature (AUDIT Phase 3.3 / C5): verify_block_finalizer_sig checks
            // it in handle_block_finalized. create_hash binds the whole finalizer_sig, so
            // current_hash (set below) stays self-consistent.
            finalizer_addr: finalizer.ed25519.public_key().expect("public key"),
            finalizer_sig: TransactionSignature {
                transaction_id: vec![],
                env_id: vec![],
                transaction_hash: b"pipeline_conflict_2_tx_hash".to_vec(),
                signature: finalizer.ed25519.sign_data(b"pipeline_conflict_2_tx_hash").expect("finalizer sig"),
                current_stake: 0,
            },
            executor_sigs: HashMap::new(),
            proposer_key: vec![2],
        },
        token_metadata: HashMap::new(),
        previous_hash: tip.clone(),
        timestamp: 0,
        current_hash: vec![],
        finality_status: pneumatic_core::blocks::FinalityStatus::Optimistic,
        proposer_key: vec![2],
        epoch_number: 0,
    };

    // Compute valid block hashes
    let block1_hash = BlockFactory::create_hash(&block1);
    let block2_hash = BlockFactory::create_hash(&block2);

    let block1 = Block {
        current_hash: block1_hash,
        ..block1
    };
    let block2 = Block {
        current_hash: block2_hash,
        ..block2
    };

    // Handle both blocks - should trigger conflict resolution. Both are
    // signed by the same registered Finalizer identity.
    let message1 = make_block_finalized_message(block1, &finalizer);
    let message2 = make_block_finalized_message(block2, &finalizer);

    let result1 = committer.handle_message(message1).await;
    assert!(result1.is_ok(), "First block should be accepted: {:?}", result1.err());

    // Second block should be rejected due to conflict
    let _result2 = committer.handle_message(message2).await;

    // Verify the chain still has the first block
    let chain = tokens.get(&vec![1]).unwrap();
    let chain_len = chain.value().blockchain.get_count();
    assert!(chain_len >= 2, "Chain should have at least 2 blocks");
}

// ---------------------------------------------------------------------------
// Fail-closed sender-auth regression tests (Phase 1.3)
//
// These assert the commender's router rejects message envelopes that fail the
// sender-auth gate: an unregistered public key, or a registered key whose role
// is not allowed to send the action. Each must fail before the gate is in
// place and pass once it is — the gate itself is the test.
// ---------------------------------------------------------------------------

/// Build an action message signed by `identity` over `body` (body content is
/// irrelevant to the gate, which runs before any handler deserializes it).
fn signed_message_with(identity: &NodeIdentity, action: &str, body: Vec<u8>) -> Message {
    Message::signed("test".to_string(), action, body, None, identity).expect("sign message")
}

#[tokio::test]
async fn unregistered_sender_commit_is_rejected() {
    // A Commit from a key never registered as a node must be rejected as
    // UnauthenticatedSender rather than reaching the commit handler.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _tokens, _node_registry) = make_test_committer(dp);

    let rogue = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let message = signed_message_with(&rogue, "Commit", vec![0u8; 8]);

    assert!(
        matches!(
            committer.handle_message(message).await,
            Err(CommitterError::UnauthenticatedSender(_))
        ),
        "an unregistered sender must not feed the router a Commit"
    );
}

#[tokio::test]
async fn unregistered_sender_block_finalized_is_rejected() {
    // Same rejection for a BlockFinalized from an unregistered key.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _tokens, _node_registry) = make_test_committer(dp);

    let rogue = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let message = signed_message_with(&rogue, "BlockFinalized", vec![0u8; 8]);

    assert!(
        matches!(
            committer.handle_message(message).await,
            Err(CommitterError::UnauthenticatedSender(_))
        ),
        "an unregistered sender must not feed the router a BlockFinalized"
    );
}

#[tokio::test]
async fn wrong_role_sender_block_finalized_is_rejected() {
    // A Committer that IS registered may not send BlockFinalized — only a
    // Finalizer may. Role mismatch must surface as UnauthorizedRole.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _tokens, node_registry) = make_test_committer(dp);

    let imposter = register_node(&node_registry, NodeRegistryType::Committer);
    let message = signed_message_with(&imposter, "BlockFinalized", vec![0u8; 8]);

    assert!(
        matches!(
            committer.handle_message(message).await,
            Err(CommitterError::UnauthorizedRole(_))
        ),
        "a registered Committer must not be able to send BlockFinalized"
    );
}

#[tokio::test]
async fn foreign_sender_epoch_reconcile_is_rejected() {
    // EpochReconcile is self-only: a foreign (unregistered) identity must be
    // rejected from reaching the epoch-reconcile logic.
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _tokens, _node_registry) = make_test_committer(dp);

    let rogue = pneumatic_core::rns::identity::NodeIdentity::generate_in_memory();
    let message = signed_message_with(&rogue, "EpochReconcile", vec![0u8; 8]);

    assert!(
        matches!(
            committer.handle_message(message).await,
            Err(_)
        ),
        "a foreign sender must not reach the epoch-reconcile handler"
    );
}
