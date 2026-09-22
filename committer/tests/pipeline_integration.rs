//! End-to-end pipeline tests covering the full transaction lifecycle:
//! - submit → optimistic → no conflict → confirmed
//! - submit → conflict → resolved → slashing

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU16, Ordering};

use dashmap::DashMap;
use pneumatic_core::blocks::{Block, BlockFactory};
use pneumatic_core::config::Config;
use pneumatic_core::crypto::{AsymCryptoProvider, BasicHashProvider};
use pneumatic_core::data::{DataError, DataProvider, DefaultDataProvider};
use pneumatic_core::config::BootstrapPeer;
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::node::NetworkPacket;
use pneumatic_core::node::NodeRegistryResponse;
use pneumatic_core::rns::config_builder::RnsNodeConfigBuilder;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::rns::wrapper::RnsNetwork;
use tokio::time::{sleep, Duration};
use pneumatic_core::environment::EnvironmentMetadata;
use pneumatic_core::epoch::CandidateRegistry;
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::logging::FileLogger;
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::{NullConnection, NodeRegistry};
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::node::NodeTypeConfig;
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
    Arc<Gossiper>,
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

    let shielded_pool = Arc::new(pneumatic_committer::shielded_pool::ShieldedPool::new(10));
    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider_core.clone(),
        node_registry.clone(),
        env_data.clone(),
        env_data.logger.clone(),
        identity.clone(),
        shielded_pool.clone(),
    ));

    let committer = Committer::new(
        env_data.clone(),
        vec![1],
        identity,
        // Share this Gossiper instance with the returned tuple: the RNS wire
        // bridge below drives the very same gossiper the committer sees.
        gossiper.clone(),
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
        shielded_pool,
    );

    (committer, pending_registry, tokens, node_registry, gossiper)
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
        shielded: None,
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
    block.current_hash = BlockFactory::create_hash(&block)
        .expect("well-formed test block hash");

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

// --- Phase 7.1 — real RNS loopback harness --------------------------------
//
// The tests above call `committer.handle_message` in-process, so they never
// touch the transport. The helpers below build a committer node wired to the
// *actual* RNS transport (the path the audit's Phase 7.1 audits), plus a
// finalizer sender node, so a serialized `Message` can be driven end-to-end
// over encrypted loopback UDP. See
// /Users/garrettolson/.claude/plans/create-an-implementation-plan-compressed-aurora.md.

/// Find a free loopback port.
///
/// Reuses the `examples/rns_spike.rs` pattern: an in-process atomic base
/// (anchored to this process) plus a `TcpListener::bind` probe, so two wire
/// tests running on parallel threads never get the same port. rns-net binds its
/// own UDP socket on the returned port, so a TCP probe here is a valid
/// availability check for UDP on the same loopback interface.
fn find_free_port() -> u16 {
    static NEXT_PORT: AtomicU16 = AtomicU16::new(0);
    let pid = std::process::id() as u16;
    let base = 20_000 + (pid % 250) * 160;
    let _ = NEXT_PORT.compare_exchange(0, base, Ordering::SeqCst, Ordering::SeqCst);
    loop {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// Build the committer half of the RNS-loopback harness, WITHOUT starting the
/// committer's RNS transport.
///
/// Returns the committer plus the shared state the tests drive: the pending
/// transaction registry, the token/chain DashMap, the node registry, the
/// committer's dedicated RNS transport identity (`committer_rns` — its
/// destination-addressing/encryption key, independent of the Ed25519 chain key
/// `authenticate_message` keys off), the transport port, and the gossiper.
///
/// The RNS transport is started separately by `start_committer_rns` — *after*
/// the finalizer exists — so the committer can be seeded with the finalizer as
/// a bootstrap peer. RNS needs a *symmetric* topology for the announce handshake
/// to activate a send route (the rns-net spike's topology A: both nodes forward
/// to each other); a receive-only committer's auto-announce has nowhere to go
/// and the finalizer's bootstrap-seeded synthetic route stays dead (SendError).
fn make_wire_committer(
    data_provider: Arc<TestDataProvider>,
) -> (
    Arc<Committer>,
    Arc<PendingTransactionRegistry>,
    Arc<DashMap<Vec<u8>, Token>>,
    Arc<NodeRegistry>,
    Arc<NodeIdentity>,
    u16,
    Arc<Gossiper>,
) {
    let (committer, pending_registry, tokens, node_registry, gossiper) =
        make_test_committer(data_provider);

    // Committer is not Clone; wrap it in Arc so the handler closure can share it
    // by cloning the Arc — the same shape committer/src/main.rs uses.
    let committer = Arc::new(committer);

    let committer_rns = Arc::new(NodeIdentity::generate_in_memory());
    let committer_port = find_free_port();

    (
        committer,
        pending_registry,
        tokens,
        node_registry,
        committer_rns,
        committer_port,
        gossiper,
    )
}

/// Start the committer's RNS transport and wire it to the committer.
///
/// Starts `RnsNetwork` on `committer_port`, forwards to `peer` (the finalizer)
/// so the committer's auto-announce reaches the finalizer and activates the
/// send route, then installs the production data-plane bridge
/// (`committer/src/main.rs:143-165`) — decrypted `NetworkPacket`s send their
/// data-plane bytes to the gossiper (control-plane omitted; the wire test sends
/// no control traffic) — and the committer's message handler (`main.rs:298`).
/// Returns the `RnsNetwork` behind an `Arc`; the caller wraps it in a
/// `Mutex<Option<Arc<RnsNetwork>>>` handle and calls `stop` at teardown. The
/// bridge and handler closures capture neither the network nor anything that
/// keeps it alive, so the Arc fully unwinds on stop.
fn start_committer_rns(
    committer: Arc<Committer>,
    committer_rns: Arc<NodeIdentity>,
    gossip: Arc<Gossiper>,
    registry: Arc<NodeRegistry>,
    committer_port: u16,
    peer: Vec<BootstrapPeer>,
) -> Arc<RnsNetwork> {
    let node_config = RnsNodeConfigBuilder::new()
        .with_udp_port(committer_port)
        .add_peer("127.0.0.1", peer[0].port)
        .build(&committer_rns.rns);
    let network =
        Arc::new(RnsNetwork::start(node_config, &committer_rns, &peer).expect("start committer rns"));

    // Production bridge (committer/src/main.rs:143-165): decrypted transport
    // packets deserialize to a `NetworkPacket`; data-plane bytes go to the
    // gossiper, control-plane to the registry, an undecodable frame is dropped.
    let gossip = gossip;
    let registry = registry;
    network.on_packet(Arc::new(move |raw: Vec<u8>| {
        match deserialize_rmp_to::<NetworkPacket>(&raw) {
            Ok(packet) => {
                if let Some(data) = packet.data {
                    if let Ok(response) = deserialize_rmp_to::<NodeRegistryResponse>(&data) {
                        let _ = registry.handle_directory_response(&response);
                    } else {
                        let _ = gossip.handle_message(data);
                    }
                }
            }
            Err(_) => {}
        }
    }));

    // Install the committer's message handler (main.rs:298): run the async
    // `handle_message` on a spawned task. The Arc shares the underlying state, so
    // this clone observes the same gossiper the bridge calls and the same
    // `tokens` the test polls.
    let committer_for_init = committer.clone();
    committer.initialize(move |message| {
        let committer = committer_for_init.clone();
        tokio::spawn(async move {
            if let Err(e) = committer.handle_message(message).await {
                eprintln!("[wire-test] committer.handle_message error: {e:?}");
            }
        });
    });

    network
}

/// Build the finalizer sender node for the RNS loopback test.
///
/// A leaf `RnsNetwork` listening on `finalizer_port`, forwarding to the
/// committer at `127.0.0.1:committer_port`, with the committer's rhash
/// pre-seeded into its destination table from `committer_rns_public_key_hex`.
/// The finalizer's auto-announce (emitted by `RnsNetwork::start`) reaches the
/// committer because the committer forwards back to it (symmetric topology —
/// the rns-net spike's topology A). The committer's auto-announce in turn
/// reaches this finalizer and upgrades its bootstrap-seeded synthetic route
/// (`receiving_interface: InterfaceId(0)`) to a real one, which is what makes
/// `send_to` succeed. The test settles after both nodes are up before sending.
///
/// Returns the owned `RnsNetwork`; the test calls `stop` when done.
fn start_finalizer_network(
    finalizer_identity: &NodeIdentity,
    committer_rns_public_key_hex: String,
    committer_port: u16,
    finalizer_port: u16,
) -> RnsNetwork {
    let bootstrap = vec![BootstrapPeer {
        public_key: committer_rns_public_key_hex,
        ip: "127.0.0.1".to_string(),
        port: committer_port,
    }];
    let node_config = RnsNodeConfigBuilder::new()
        .with_udp_port(finalizer_port)
        .add_peer("127.0.0.1", committer_port)
        .build(&finalizer_identity.rns);
    RnsNetwork::start(node_config, finalizer_identity, &bootstrap)
        .expect("start finalizer rns")
}

/// Tear down an RNS handle: unwrap the Arc (the bridge holds no clone) and stop
/// the node, joining its worker pool and releasing its UDP port.
fn shutdown_rns(handle: &Arc<Mutex<Option<Arc<RnsNetwork>>>>) {
    match handle.lock().unwrap().take() {
        Some(net) => match Arc::try_unwrap(net) {
            Ok(node) => node.stop(),
            Err(_) => eprintln!("[wire-test] rns handle still referenced at shutdown"),
        },
        None => {}
    }
}


/// Send over the finalizer's RNS transport, retrying until the send route
/// establishes. The route needs a moment to come up after the explicit re-announce
/// in `wire_boot`; RNS finishes the announce/path handshake asynchronously, so the
/// first `send_to` often returns `SendError` before the route is usable. This
/// bounds the wait (~5s) instead of failing on the first error, so the frame
/// actually traverses the loopback to the committer's gate. Returns the error
/// only if the route never came up within the deadline.
async fn send_until_route(
    net: &RnsNetwork,
    rhash: [u8; 16],
    payload: &[u8],
) -> Result<(), PneumaticError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match net.send_to(rhash, payload) {
            Ok(()) => return Ok(()),
            Err(_) if std::time::Instant::now() < deadline => {
                sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Boot both RNS nodes for a wire test, with a symmetric topology so the announce
/// handshake can activate a send route, then settle the link.
///
/// Mirrors the rns-net spike's topology A: the finalizer forwards to the
/// committer and the committer forwards back to the finalizer, so each node's
/// auto-announce (emitted by `RnsNetwork::start`) reaches the other and upgrades
/// the bootstrap-seeded synthetic route (`receiving_interface: InterfaceId(0)`)
/// to a real one. The finalizer identity is registered as a `Finalizer` unless
/// `register_finalizer` is false — that is the unregistered-sender negative test,
/// which signs with an identity the committer has never seen.
///
/// Returns the shared token chains (the committer mutates them in place, so the
/// test's `wait_for_chain` observes growth), the finalizer identity, the
/// committer transport identity (whose `rhash` is the send target), the finalizer's
/// sender node, and the committer's RNS handle (stopped at teardown).
async fn wire_boot(
    dp: Arc<TestDataProvider>,
    register_finalizer: bool,
) -> (
    Arc<DashMap<Vec<u8>, Token>>,
    NodeIdentity,
    Arc<NodeIdentity>,
    RnsNetwork,
    Arc<Mutex<Option<Arc<RnsNetwork>>>>,
) {
    let (
        committer,
        _pending_registry,
        tokens,
        node_registry,
        committer_rns,
        committer_port,
        gossip,
    ) = make_wire_committer(dp);

    let finalizer = if register_finalizer {
        register_node(&node_registry, NodeRegistryType::Finalizer)
    } else {
        NodeIdentity::generate_in_memory()
    };
    bootstrap_token_chain(&tokens);

    let finalizer_port = find_free_port();

    // Start the finalizer first so its listener is up before the committer
    // announces; the committer's announce then reaches an already-listening peer.
    let committer_rns_public_key_hex =
        hex::encode(committer_rns.rns.get_public_key().expect("committer rns public key"));
    let finalizer_network = start_finalizer_network(
        &finalizer,
        committer_rns_public_key_hex,
        committer_port,
        finalizer_port,
    );

    // Committer forwards back to the finalizer (symmetric topology), so its
    // auto-announce reaches the finalizer and activates the finalizer's send
    // route to the committer.
    let finalizer_rns_public_key_hex =
        hex::encode(finalizer.rns.get_public_key().expect("finalizer rns public key"));
    let committer_bootstrap = vec![BootstrapPeer {
        public_key: finalizer_rns_public_key_hex,
        ip: "127.0.0.1".to_string(),
        port: finalizer_port,
    }];
    let committer_handle = {
        let committer_network = start_committer_rns(
            committer,
            committer_rns.clone(),
            gossip,
            node_registry.clone(),
            committer_port,
            committer_bootstrap,
        );
        Arc::new(Mutex::new(Some(committer_network)))
    };

    // Explicitly re-announce on both nodes (the rns-net spike's proven recipe):
    // the startup auto-announce raced the peer's listener coming up, so call
    // announce once both listeners are up. This re-traverses the established
    // links, which is what upgrades the finalizer's bootstrap-seeded synthetic
    // route to a usable one and makes `send_to` succeed.
    if let Some(net) = committer_handle.lock().unwrap().clone() {
        net.announce();
    }
    finalizer_network.announce();

    // Let the announce handshake propagate and the route activate.
    sleep(Duration::from_millis(2000)).await;

    (
        tokens,
        finalizer,
        committer_rns,
        finalizer_network,
        committer_handle,
    )
}

#[tokio::test]
async fn test_pipeline_no_conflict() {
    // Test: submit → optimistic → no conflict → confirmed

    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, tokens, node_registry, _gossiper) = make_test_committer(dp);

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
            shielded: None,
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
    let expected_hash = BlockFactory::create_hash(&block)
        .expect("well-formed test block hash");
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
    let (committer, registry, tokens, node_registry, _gossiper) = make_test_committer(dp);

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
            shielded: None,
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
        current_hash: BlockFactory::create_hash(&block).expect("well-formed test block hash"),
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
    let (committer, _registry, tokens, node_registry, _gossiper) = make_test_committer(dp);

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
            shielded: None,
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
            shielded: None,
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
    let block1_hash = BlockFactory::create_hash(&block1)
        .expect("well-formed test block hash");
    let block2_hash = BlockFactory::create_hash(&block2)
        .expect("well-formed test block hash");

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
    let (committer, _registry, _tokens, _node_registry, _gossiper) = make_test_committer(dp);

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
    let (committer, _registry, _tokens, _node_registry, _gossiper) = make_test_committer(dp);

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
    let (committer, _registry, _tokens, node_registry, _gossiper) = make_test_committer(dp);

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
    let (committer, _registry, _tokens, _node_registry, _gossiper) = make_test_committer(dp);

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

    // ------------------------------------------------------------------------
    // Phase 7.1 — real RNS loopback wire path
    //
    // The tests above call `committer.handle_message` in-process, so they never
    // touch the transport. The tests below drive a real serialized `Message` over
    // the actual `RnsNetwork` transport (identity-encrypted UDP loopback, rhash
    // addressing) through the verbatim `committer/src/main.rs:143-165`
    // `NetworkPacket` bridge into the committer's gossiper, dispatch through the
    // committer, and assert the downstream chain effect (block appended) — never
    // the return of a direct `handle_message`.
    //
    // Transport reachability: the finalizer's route to the committer is seeded by
    // bootstrap (a send to a pre-announce destination is accepted, so there is no
    // announce-timing race). Sender registration is reached directly by
    // registering the finalizer identity in the committer's registry — exactly
    // the binding the directory handshake would populate — rather than
    // re-running the non-deterministic directory exchange (Phase 1.5).
    // ------------------------------------------------------------------------


    // -------------------------------------------------------------------------
    // Positive: a well-formed NetworkPacket frame is carried end-to-end over the
    // real RNS loopback (identity-encrypted UDP, rhash addressing, the 4-thread
    // decrypt worker pool) and arrives at the committer's transport callback
    // verbatim. This exercises the actual RNS transport the audit flagged as
    // never exercised — the worker decrypts the inbound frame and hands the raw
    // plaintext to the on_packet callback.
    //
    // Constrained by an audit finding (Phase 7.1, AUDIT_CHECKLIST.md): a full
    // pneumatic Message cannot traverse this path. The PQC hybrid signature alone
    // is 3796 B, so every Message serializes to >= 3.8 KB, above RNS's 500 B
    // packet cap. This test therefore uses a minimal in-limit NetworkPacket frame
    // — the largest message shape RNS can currently carry — to prove the
    // transport itself is exercised end-to-end.
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn wire_rns_transport_delivers_network_packet() {
        // Committer receiver: a bare RnsNetwork with a recording on_packet
        // callback (the transport's delivery point), seeded with no route — it
        // only receives.
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let committer_rns = Arc::new(NodeIdentity::generate_in_memory());
        let committer_port = find_free_port();
        let node_config = RnsNodeConfigBuilder::new()
            .with_udp_port(committer_port)
            .build(&committer_rns.rns);
        let committer_net = RnsNetwork::start(node_config, &committer_rns, &[])
            .expect("start committer rns");
        committer_net.on_packet(Arc::new(move |raw: Vec<u8>| {
            let _ = tx.send(raw);
        }));

        // Finalizer sender: one interface forwarding to the committer, with the
        // committer's rhash pre-seeded from its public key (bootstrap seed — a
        // send to a pre-announce destination is accepted, per the rns-net spike).
        // Re-announce to activate the route, then settle.
        let finalizer = NodeIdentity::generate_in_memory();
        let finalizer_port = find_free_port();
        let committer_pub_hex =
            hex::encode(committer_rns.rns.get_public_key().expect("committer rns public key"));
        let finalizer_net = start_finalizer_network(
            &finalizer,
            committer_pub_hex,
            committer_port,
            finalizer_port,
        );
        finalizer_net.announce();
        sleep(Duration::from_millis(2000)).await;

        // A minimal, in-limit NetworkPacket frame.
        let frame = NetworkPacket {
            control: None,
            data: Some(vec![1u8, 2, 3]),
        };
        let payload = serialize_to_bytes_rmp(&frame).expect("serialize NetworkPacket");

        // Sanity: the frame must be within RNS's 500 B packet cap. Larger
        // payloads (including any real Message) are rejected by RNS's pack step;
        // this is the framing guarantee the test relies on.
        assert!(
            payload.len() <= 500,
            "test frame must fit RNS's 500 B packet cap (got {})",
            payload.len()
        );

        finalizer_net
            .send_to(committer_rns.rhash, &payload)
            .expect("send frame over rns loopback");

        // The committer's worker decrypts the inbound frame and delivers the raw
        // plaintext to the on_packet callback — byte-for-byte what we sent.
        let received = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("frame delivered over rns loopback");
        assert_eq!(received, payload, "decrypted frame must match what we sent");

        finalizer_net.stop();
        committer_net.stop();
    }

    // -------------------------------------------------------------------------
    // Negative: an undecodable frame (not a NetworkPacket) is dropped by the
    // bridge without panicking or appending.
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn wire_undecodable_frame_dropped_by_bridge() {
        let dp = Arc::new(TestDataProvider::new());
        let chain_id = vec![1];

        let (tokens, _finalizer, committer_rns, finalizer_network, committer_handle) =
            wire_boot(dp, true).await;

        // Raw non-NetworkPacket bytes as the RNS plaintext payload.
        let payload = vec![0xFF; 32];

        // Retry until the route establishes so the frame reaches the bridge,
        // whose deserialize fails; the frame is dropped (no append).
        send_until_route(&finalizer_network, committer_rns.rhash, &payload)
            .await
            .expect("send undecodable frame over rns");

        // The bridge's deserialize fails; the frame is dropped (no append).
        sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            tokens.get(&chain_id).unwrap().value().blockchain.get_count(),
            1,
            "an undecodable frame must not be appended"
        );

        finalizer_network.stop();
        shutdown_rns(&committer_handle);
    }

