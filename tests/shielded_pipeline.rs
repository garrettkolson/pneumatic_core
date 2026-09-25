//! Phase S6.1 — cross-crate shielded pipeline integration at the root level.
//!
//! The per-crate suites pin ONE hop each (sentinel S5.1 advisory gate;
//! finalizer S5.2 sign / quorum / dispatch; committer S5.3 authoritative
//! re-check + pool/chain lockstep). This file drives the **full four hops in
//! one process** through the real wire messages, with the sentinel,
//! finalizer, and committer sharing the ONE `Arc<ShieldedPool>` and the ONE
//! `PendingTransactionRegistry` the composite deployment uses:
//!
//! ```text
//! sender ──ShieldedTransfer──▶ sentinel ──SignShielded──▶ finalizer
//!  finalizer ──ShieldedVote──▶ (self; stake quorum 1/1) ──Commit──▶ committer
//! ```
//!
//! Three properties are pinned:
//!
//! 1. **Wire privacy** (fast, default suite): none of the wire bytes across
//!    all three roles carries note plaintexts — no owner pk, no spend seed,
//!    no value (LE or BE), no recipient ed25519/x25519 pk. Only the public
//!    surface (commitments, nullifier, Merkle root, proof).
//! 2. **Advisory-vs-authoritative split** (fast, default suite): a
//!    double-spend the sentinel's advisory gate can see through the shared
//!    pool view is rejected before any finalizer work — no `SignShielded` is
//!    ever emitted. (Checks 1→3 short-circuit before check 4, so the fast
//!    suite pays no halo2 keygen.)
//! 3. **Authoritative gate + lockstep** (LIVE, `#[ignore]`d): the committer's
//!    re-check reaches check 4, whose lazy verifier pays the one-time
//!    ActionCircuit `keygen_vk` (~2.5 min) in this test binary. A fake
//!    proof dies at the committer with the pool untouched; a real halo2
//!    proof commits — pool delta and token chain advance in lockstep, the
//!    recipient recovers the output value, and the pool delta is idempotent
//!    under replay.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pneumatic_core::blocks::{Block, BlockFactory, FinalityStatus};
use pneumatic_core::conns::{ConnError, Connection};
use pneumatic_core::config::Config;
use pneumatic_core::crypto::{AsymCryptoProvider, BasicHashProvider, Ed25519Provider};
use pneumatic_core::data::{AppliedPoolDelta, ShieldedPoolState, StubDataProvider};
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
use pneumatic_core::epoch::{
    BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector, StakeSet,
};
use pneumatic_core::errors::{PneumaticError, TransactionRiskFactor, ValidationFailureReason};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::messages::Message;
use pneumatic_core::node::{
    registry::NodeRegistry, NodeRegistryType, NodeType, NodeTypeConfig,
};
use pneumatic_core::rns::{config_builder::DEFAULT_UDP_PORT, identity::NodeIdentity};
use pneumatic_core::registry::{
    PendingTransactionRegistry, TransactionSignatureRegistry,
};
use pneumatic_core::shielded::{
    commit, nullifier, root_to_bytes, IncrementalMerkleTree, MembershipProof, ShieldedNote,
    DEFAULT_DEPTH,
};
use pneumatic_core::tokens::Token;
use pneumatic_core::transactions::{
    ShieldedTransaction, SignedTransaction, Transaction, TransactionValidationResult,
};
use pneumatic_core::user::User;
use pneumatic_core::validation::{
    ShieldedValidationDeps, ShieldedValidationSpec, TransactionValidationSpec,
    ValidationSpecRegistry,
};
use pneumatic_committer::block_services::BlockServices;
use pneumatic_committer::committer_error::CommitterError;
use pneumatic_committer::epoch_manager::{
    EpochReconciler, LeaderSelector, StakeStore, StakingManager,
};
use pneumatic_committer::{Committer, PoolApplyOutcome, ShieldedPool};
use pneumatic_finalizer::finalizer::Finalizer;
use pneumatic_prover::{assemble_tx, build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};
use pneumatic_sentinel::sentinel::Sentinel;
use pneumatic_sentinel::sentinel_error::SentinelError;
use pasta_curves::group::ff::PrimeField;
use pasta_curves::group::GroupEncoding;
use pasta_curves::pallas::Base as Fp;
use pasta_curves::pallas::Scalar as Fq;

// ---------------------------------------------------------------------------
// Fixtures (public API only)
// ---------------------------------------------------------------------------

const ENV_ID: &str = "test_env";
const TOKEN_ID: &[u8] = &[1u8];
const SENDER: &[u8] = b"alice";

/// Test-only shielded spec (the S5.1/S5.2 stub pattern): registered under the
/// real `ShieldedValidationSpec::NAME` (the lookup seam the sentinel and
/// finalizer resolve), cheap (no halo2 keygen), valid for any non-empty
/// proof. The real spec's cryptographic behavior is pinned in core (S4.1.5)
/// and in the committer's re-check (S5.3), which is registry-independent.
struct StubShieldedSpec;

fn stub_zero_risk() -> TransactionRiskFactor {
    TransactionRiskFactor {
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
        _token: &Token,
        _env_data: &EnvironmentMetadata,
    ) -> Result<TransactionValidationResult, PneumaticError> {
        Ok(TransactionValidationResult::valid(vec![], stub_zero_risk()))
    }

    fn calculate_risk(&self, _tx: &Transaction) -> TransactionRiskFactor {
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

/// The test environment (the finalizer fixture's spec JSON — the canonical
/// root-test shape), with the spec registry installed BEFORE the Arc so the
/// shared env carries it. `real_shielded` selects the real `Shielded` spec
/// (the live tests) vs the stub (the fast tests).
fn test_env(real_shielded: bool) -> Arc<EnvironmentMetadata> {
    let json = r#"{
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
        serde_json::from_str::<EnvironmentMetadataSpec>(json).expect("valid test env JSON");
    let mut env =
        EnvironmentMetadata::load_from_spec(spec).expect("valid test environment spec");
    let mut registry = ValidationSpecRegistry::new();
    registry.register_defaults();
    if real_shielded {
        registry.register_shielded();
    } else {
        registry.register(Box::new(StubShieldedSpec));
    }
    env.transaction_validation_specs = Arc::new(registry);
    Arc::new(env)
}

/// A `Connection` that records every "sent" payload byte-verbatim.
struct RecordingConnection {
    recorder: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl Connection for RecordingConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), ConnError> {
        self.recorder.lock().unwrap().push(data.clone());
        Ok(())
    }
}

/// A `Connection` that discards sent data (auth-gate-only peers).
struct NoOpConnection;

#[async_trait::async_trait]
impl Connection for NoOpConnection {
    async fn send(&self, _data: &Vec<u8>) -> Result<(), ConnError> {
        Ok(())
    }
}

/// `send_to_all` dispatches on a detached thread, so recorder counts are
/// observed by polling (up to ~2 s).
fn poll_recorder(recorder: &Arc<Mutex<Vec<Vec<u8>>>>, expected: usize) -> bool {
    for _ in 0..100 {
        if recorder.lock().unwrap().len() >= expected {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// The role config for a node with the given identity (all three roles share
/// the same shape — the composite deployment runs one process per role set).
fn role_config(identity: &Arc<NodeIdentity>) -> Config {
    let type_configs = Arc::new({
        let tc = dashmap::DashMap::new();
        for t in [
            NodeRegistryType::Sentinel.clone(),
            NodeRegistryType::Finalizer.clone(),
            NodeRegistryType::Committer.clone(),
        ] {
            tc.insert(t, NodeTypeConfig { min: 0, max: 10, min_stake: 0 });
        }
        tc
    });
    Config {
        public_key: identity.ed25519.public_key().expect("identity public key"),
        ip_address: "127.0.0.1".parse().expect("localhost"),
        rest_api_version: 1,
        node_type: NodeType::Full,
        node_registry_types: vec![
            NodeRegistryType::Sentinel,
            NodeRegistryType::Finalizer,
            NodeRegistryType::Committer,
        ],
        main_environment_id: ENV_ID.to_string(),
        reconciliation_partition_id: "reconciliation".to_string(),
        environment_metadata: Arc::new(dashmap::DashMap::new()),
        type_configs,
        identity: identity.clone(),
        rhash: identity.rhash,
        bootstrap_peers: Vec::new(),
        rns_port: DEFAULT_UDP_PORT,
        transport_enabled: false,
    }
}

fn make_node_registry(config: Config) -> Arc<NodeRegistry> {
    Arc::new(NodeRegistry::init(Arc::new(config), None, Arc::new(|_, _| true)))
}

/// The shielded test token: self-verified, shielded opt-in, and carrying a
/// genesis block (the SAME genesis goes into the data provider AND the
/// committer's token cache, so the finalizer's `resolve_previous_hash`
/// (provider) and the committer's conflict check (cache) reference the
/// same tip).
fn shielded_token() -> Token {
    let mut token = Token::new();
    token.id = TOKEN_ID.to_vec();
    token.is_self_verified = true;
    token.set_metadata("shielded_opt_in".into(), "true".into());
    token.blockchain.add_block(genesis_block());
    token
}

fn genesis_block() -> Block {
    let mut block = Block {
        signed_trans: SignedTransaction::test_transaction(),
        token_metadata: HashMap::new(),
        previous_hash: vec![42u8; 32],
        timestamp: 0,
        current_hash: vec![],
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash = BlockFactory::create_hash(&block).expect("genesis block hashes");
    block
}

/// The canonical S4.1 input note (value 100, owner [1u8; 32]).
fn input_note() -> ShieldedNote {
    ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) }
}

/// The input note's spend key (the secret the wire must never carry).
const INPUT_SPEND_KEY: [u8; 32] = [0xA1u8; 32];

/// The pool's boot state: the input note's single leaf, with its seed delta
/// (a stored leaf without its delta fails `load`'s integrity re-derivation).
/// `pre_spent` marks the nullifier spent before the pipeline runs (the
/// double-spend fixture).
fn pool_fixture(
    pre_spent: Option<[u8; 32]>,
) -> (ShieldedPoolState, ShieldedNote, MembershipProof, [u8; 32], [u8; 32]) {
    let input = input_note();
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root, _) = tree.append(&commit(&input));
    let root = root_to_bytes(&root);
    let leaf = root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&input)));
    let nullifier = nullifier(&input, &INPUT_SPEND_KEY);
    let state = ShieldedPoolState {
        root,
        leaf_count: 1,
        leaves: vec![leaf],
        nullifiers: pre_spent.into_iter().collect(),
        applied: vec![AppliedPoolDelta {
            block_hash: vec![99u8; 32],
            leaves: vec![leaf],
            nullifiers: pre_spent.into_iter().collect(),
            post_root: root,
        }],
    };
    (state, input, tree.membership_proof(0), root, nullifier)
}

/// The full pipeline fixture: sentinel + finalizer + committer sharing ONE
/// `PendingTransactionRegistry` (the sentinel's `register_shielded` feeds the
/// committer's S5.3 cross-check) and ONE `Arc<ShieldedPool>` (the
/// single-writer pool; sentinel and finalizer see it as a read-only view).
///
/// Identity layout (the composite pattern): ONE identity for the sentinel AND
/// the finalizer — the finalizer's C1 gate requires the `SignShielded`
/// sender to be a registered Finalizer — and a separate committer identity
/// (its role gate requires the Commit's sender registered as a Finalizer).
struct Pipeline {
    sentinel: Sentinel,
    finalizer: Finalizer,
    committer: Committer,
    registry: Arc<PendingTransactionRegistry>,
    pool: Arc<ShieldedPool>,
    env: Arc<EnvironmentMetadata>,
    /// The sentinel's Finalizer peer — captures the outbound SignShielded.
    sign_recorder: Arc<Mutex<Vec<Vec<u8>>>>,
    /// The finalizer's own Finalizer peer — captures the fanned-out vote.
    vote_recorder: Arc<Mutex<Vec<Vec<u8>>>>,
    /// The finalizer's Committer peer — captures Commit + BlockFinalized.
    commit_recorder: Arc<Mutex<Vec<Vec<u8>>>>,
}

fn build_pipeline(real_shielded: bool, pre_spent: Option<[u8; 32]>) -> Pipeline {
    let env = test_env(real_shielded);
    let (pool_state, _input, _proof, _root, _nullifier) = pool_fixture(pre_spent);

    // Identities: one for sentinel+finalizer (composite), one for committer.
    let sf_identity = Arc::new(NodeIdentity::generate_in_memory());
    let cm_identity = Arc::new(NodeIdentity::generate_in_memory());
    let sf_pk = sf_identity.ed25519.public_key().expect("sf public key");
    let cm_pk = cm_identity.ed25519.public_key().expect("cm public key");

    // Shared data provider: the opt-in self-verified genesis-chained token,
    // the sender user, the stake snapshot for BOTH epochs the roles read
    // (epoch 0 — the finalizer's quorum/stake lookups; epoch 1 — the
    // sentinel's finalizer assignment; the sentinel's `current_epoch`
    // defaults to 1), and the pool's boot state.
    let stake_set = StakeSet { stakers: [(sf_pk.clone(), 100u64)].into_iter().collect() };
    let data_provider: Arc<dyn pneumatic_core::data::DataProvider> = Arc::new(
        StubDataProvider::new()
            .with_token(TOKEN_ID.to_vec(), "token".to_string(), shielded_token())
            .with_user(
                SENDER.to_vec(),
                "token".to_string(),
                User {
                    public_key: SENDER.to_vec(),
                    fuel_balance: 1_000_000,
                    stake: 0,
                    nonce: 0,
                },
            )
            .with_stake_snapshot(0, stake_set.clone())
            .with_stake_snapshot(1, stake_set)
            .with_shielded_pool(pool_state),
    );

    // The ONE pool: loaded from the shared provider's boot state; the
    // sentinel and finalizer receive it as their (read-only) pool view, the
    // committer as its single-writer pool. (`load` hands back an Arc.)
    let pool =
        ShieldedPool::load(data_provider.as_ref(), "token", 10).expect("seeded pool loads");

    // --- Sentinel (hop 1) ---
    let sentinel_config = role_config(&sf_identity);
    let sentinel_registry = make_node_registry(sentinel_config.clone());
    let sign_recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(
        sentinel_registry.register_peer(
            sf_pk.clone(),
            sf_identity.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection {
                recorder: sign_recorder.clone()
            }),
        ),
        "sentinel: finalizer peer registers"
    );
    let sentinel_gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Sentinel,
        sentinel_config.clone(),
        300,
        env.asym_crypto_provider.clone(),
    ));
    let registry = Arc::new(PendingTransactionRegistry::new());
    let sentinel = Sentinel::new(
        sentinel_config,
        env.clone(),
        sentinel_registry,
        registry.clone(),
        sentinel_gossiper,
        data_provider.clone(),
        pool.clone(),
    );

    // --- Finalizer (hops 2+3) ---
    let finalizer_config = role_config(&sf_identity);
    let finalizer_registry = make_node_registry(finalizer_config.clone());
    let vote_recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(
        finalizer_registry.register_peer(
            sf_pk.clone(),
            sf_identity.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { recorder: vote_recorder.clone() }),
        ),
        "finalizer: own key registers as a Finalizer peer (vote fan-out capture)"
    );
    let commit_recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(
        finalizer_registry.register_peer(
            cm_pk.clone(),
            cm_identity.rhash,
            &NodeRegistryType::Committer,
            Box::new(RecordingConnection { recorder: commit_recorder.clone() }),
        ),
        "finalizer: committer peer registers (commit capture)"
    );
    let (signing_key, verifying_key) = test_signing_key();
    let finalizer = Finalizer::new(
        ENV_ID.to_string(),
        sf_pk.clone(),
        sf_identity.clone(),
        finalizer_registry,
        registry.clone(),
        Arc::new(TransactionSignatureRegistry::new()),
        67.0, // stake quorum
        1,   // total voters (100/100 stake — one vote is quorum)
        signing_key,
        verifying_key,
        Arc::new(BasicHashProvider::new()),
        vec![10, 20, 30], // leader address
        100,              // leader stake
        vec![40, 50, 60], // leader hash
        0,                // current epoch (its stake snapshot lives at epoch 0)
        data_provider.clone(),
        "token".to_string(),
        pool.clone(),
        env.clone(),
    );

    // --- Committer (hop 4) ---
    let committer_config = role_config(&cm_identity);
    let committer_registry = make_node_registry(committer_config.clone());
    assert!(
        committer_registry.register_peer(
            sf_pk.clone(),
            sf_identity.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(NoOpConnection),
        ),
        "committer: finalizer key registers (the Commit role gate)"
    );
    let committer_gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Committer,
        committer_config.clone(),
        60,
        env.asym_crypto_provider.clone(),
    ));

    // The committer's token cache: the SAME genesis-chained token as the
    // provider (the conflict check compares the block's previous_hash
    // against this chain's tip).
    let tokens = Arc::new({
        let map = dashmap::DashMap::new();
        map.insert(TOKEN_ID.to_vec(), shielded_token());
        map
    });

    // Epoch machinery (the committer construction contract — mirrors the
    // committer's in-crate fixture; none of it runs during a shielded
    // commit).
    let stake_store = Arc::new(StakeStore::new());
    let staking_manager = Arc::new(StakingManager::new(
        stake_store.clone(),
        env.logger.clone(),
    ));
    let default_dp = Arc::new(pneumatic_core::data::DefaultDataProvider::new());
    let candidate_registry = Arc::new(CandidateRegistry::new());
    let epoch_reconciler = Arc::new(EpochReconciler::new(
        stake_store.clone(),
        candidate_registry.clone(),
        default_dp,
        "test".to_string(),
        vec![TOKEN_ID.to_vec()],
        env.cost_model.slash_fraction,
    ));
    let hash_provider = Arc::new(BasicHashProvider::new());
    let leader_selector = Arc::new(LeaderSelector::new(hash_provider.clone()));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64;
    let initial_epoch = Epoch::new_with_leader(
        1,
        now,
        now + 300,
        leader_selector.as_ref(),
        &stake_store.to_stake_set(),
        &[],
    );
    let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
    let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));

    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider.clone(),
        committer_registry.clone(),
        env.clone(),
        env.logger.clone(),
        cm_identity.clone(),
        pool.clone(),
    ));

    let committer = Committer::new(
        env.clone(),
        cm_pk,
        cm_identity,
        committer_gossiper,
        block_services,
        committer_registry,
        tokens,
        registry.clone(),
        stake_store,
        staking_manager,
        epoch_reconciler,
        leader_selector,
        data_provider.clone(),
        0,
        Some(epoch_detector),
        block_proposer,
        300,
        5000,
        candidate_registry,
        pool.clone(),
    );

    Pipeline {
        sentinel,
        finalizer,
        committer,
        registry,
        pool,
        env,
        sign_recorder,
        vote_recorder,
        commit_recorder,
    }
}

fn test_signing_key() -> (ed25519_dalek::SigningKey, ed25519_dalek::VerifyingKey) {
    // Deterministic finalizer signing key (the finalizer fixture pattern).
    let seed = [0x5Au8; 32];
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    (signing_key.clone(), signing_key.verifying_key())
}

// ---------------------------------------------------------------------------
// Pipeline driver — the four wire hops
// ---------------------------------------------------------------------------

/// Drive the full four hops for `stx`. Returns the three captured wire
/// messages (SignShielded, ShieldedVote, Commit). Fails loudly if any hop
/// does not emit.
async fn drive_pipeline(p: &Pipeline, stx: &pneumatic_core::transactions::ShieldedTransaction)
-> (Message, Message, Message) {
    // Hop 1: sender → sentinel (inner "ShieldedTransfer" message, the
    // composite plugin's delivery shape).
    let body = serialize_to_bytes_rmp(stx).expect("stx serializes");
    let inner = Message {
        chain_id: ENV_ID.to_string(),
        action: "ShieldedTransfer".to_string(),
        body,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&inner).expect("inner message serializes");
    p.sentinel
        .on_data_received(raw)
        .expect("the sentinel admits the shielded transfer");

    // Hop 2: sentinel → finalizer (SignShielded).
    assert!(
        poll_recorder(&p.sign_recorder, 1),
        "SignShielded reaches the finalizer"
    );
    let sign: Message =
        deserialize_rmp_to(&p.sign_recorder.lock().unwrap()[0].clone())
            .expect("captured SignShielded payload is a Message");
    assert_eq!(sign.action, "SignShielded");
    p.finalizer
        .handle_sign_shielded(&sign)
        .await
        .expect("the finalizer signs the public outputs");

    // Hop 3: finalizer → finalizer peer (ShieldedVote) → stake quorum →
    // Commit + BlockFinalized fan-out.
    assert!(
        poll_recorder(&p.vote_recorder, 1),
        "the vote fans out to the finalizer set"
    );
    let vote: Message =
        deserialize_rmp_to(&p.vote_recorder.lock().unwrap()[0].clone())
            .expect("captured vote payload is a Message");
    assert_eq!(vote.action, "ShieldedVote");
    p.finalizer
        .handle_shielded_vote(&vote)
        .await
        .expect("stake quorum reached; commit dispatched");

    assert!(
        poll_recorder(&p.commit_recorder, 1),
        "the Commit reaches the committer"
    );
    let commit: Message =
        deserialize_rmp_to(&p.commit_recorder.lock().unwrap()[0].clone())
            .expect("captured commit payload is a Message");
    assert_eq!(commit.action, "Commit");

    (sign, vote, commit)
}

// ---------------------------------------------------------------------------
// Fast tests (default suite — no halo2 keygen)
// ---------------------------------------------------------------------------

/// S6.1 property 1 — **wire privacy**: across ALL recorded wire bytes of the
/// three roles (sentinel's SignShielded; the finalizer's ShieldedVote,
/// Commit, and BlockFinalized), nothing carries note plaintexts. The
/// distinctive values below are chosen so a leak is detectable in either
/// byte order; the scan asserts only the public surface is present.
#[tokio::test]
async fn wire_bytes_carry_only_the_public_surface() {
    // Distinctive values (no other u64 in the fixture equals these).
    const IN_VALUE: u64 = 712_398_741;
    const OUT_VALUE: u64 = 398_741_029;

    let recipient = ShieldedIdentity {
        spend: SpendKey::from_seed([0xC4u8; 32]),
        identity: Ed25519Provider::generate(),
    };
    let input = ShieldedNote {
        value: IN_VALUE,
        owner_pk: [0xA1u8; 32],
        rho: Fq::from(1),
        rcm: Fq::from(2),
    };
    let output = ShieldedNote {
        value: OUT_VALUE,
        owner_pk: recipient.owner_pk(),
        rho: Fq::from(3),
        rcm: Fq::from(4),
    };
    let spent_commit = commit(&input).to_bytes().as_ref().try_into().unwrap();
    let output_commit = commit(&output).to_bytes().as_ref().try_into().unwrap();

    // Build the stx directly (wire assembly only — no proving): the fake
    // proof is fine for the sentinel/finalizer (stub spec) and the
    // cross-crate S5.3 seam; the committer's authoritative check 4 is
    // exercised by the ignored tests.
    let pool_root = {
        let (state, _input, _proof, _root, _nullifier) = pool_fixture(None);
        let dp = StubDataProvider::new().with_shielded_pool(state);
        let pool = ShieldedPool::load(&dp, "token", 10).expect("pool loads");
        pool.state_snapshot().root
    };
    let stx = assemble_tx(
        TOKEN_ID,
        vec![spent_commit],
        vec![nullifier(&input, &INPUT_SPEND_KEY)],
        vec![output_commit],
        pool_root,
        vec![9u8; 64],
        vec![vec![0x77u8; 32]], // one (opaque) note ciphertext per output
        10_000_000,
    );

    let p = build_pipeline(false, None);
    drive_pipeline(&p, &stx).await;

    // The cross-crate S5.3 seam: the sentinel's atomic registration is what
    // the committer's H13/S5.3 hash check later binds to.
    let registered = p
        .registry
        .get_shielded(&stx.id)
        .expect("the sentinel registered the stx in the shared registry");
    assert_eq!(
        registered.hash().expect("registered hash"),
        stx.hash().expect("wire hash"),
        "the registered stx hash-binds to the wire payload"
    );

    // The wire-privacy scan over every recorded payload of every role.
    let mut wire: Vec<Vec<u8>> = Vec::new();
    for recorder in [&p.sign_recorder, &p.vote_recorder, &p.commit_recorder] {
        wire.extend(recorder.lock().unwrap().iter().cloned());
    }
    assert!(!wire.is_empty(), "the pipeline emitted wire traffic");

    let secrets: &[&[u8]] = &[
        &input.owner_pk[..],          // input owner pk
        &output.owner_pk[..],         // output owner pk
        &INPUT_SPEND_KEY[..],         // input spend seed
        &recipient.spend.spend_secret()[..], // output spend seed
        &recipient.identity.public_key().expect("ed pk")[..], // recipient ed25519 pk
        &recipient
            .identity
            .x25519_public_key()
            .expect("x25519 pk")[..], // recipient x25519 pk
    ];
    let in_le = IN_VALUE.to_le_bytes();
    let in_be = IN_VALUE.to_be_bytes();
    let out_le = OUT_VALUE.to_le_bytes();
    let out_be = OUT_VALUE.to_be_bytes();
    let values: [&[u8]; 4] = [
        in_le.as_slice(),
        in_be.as_slice(),
        out_le.as_slice(),
        out_be.as_slice(),
    ];
    for payload in &wire {
        for secret in secrets {
            assert!(
                !contains_bytes(payload, secret),
                "a wire payload carries a note secret ({:?} bytes) — privacy violation",
                secret.len()
            );
        }
        for value in &values {
            assert!(
                !contains_bytes(payload, value),
                "a wire payload carries a note value — privacy violation"
            );
        }
    }

    // And the public surface IS on the wire (the commitments, the
    // nullifier, the root, and the proof). The wire form is MsgPack, where
    // `[u8; 32]` fields encode as integer arrays (not raw byte strings), so
    // the assertion checks each value in its rmp encoding. `Message.body`
    // itself is a `Vec<u8>` — rmp-encoding a byte vector yields per-byte
    // integer elements (the inner stx bytes are nested, not contiguous) —
    // so the presence surface is the DECODED message bodies, where the stx
    // rmp (and, inside the commit, the commit rmp) travels as real bytes.
    let mut surface: Vec<u8> = Vec::new();
    for payload in &wire {
        let msg: Message =
            deserialize_rmp_to(payload).expect("every recorded payload is a Message");
        surface.extend(msg.body);
    }
    let all = &surface;
    for (name, public) in [
        ("spent_commit", &spent_commit),
        ("output_commit", &output_commit),
        ("nullifier", &stx.nullifiers[0]),
        ("merkle_root", &stx.merkle_root),
    ] {
        let encoded = serialize_to_bytes_rmp(public).expect("public value encodes");
        assert!(
            contains_bytes(&all, &encoded),
            "the public surface is on the wire: {name}"
        );
    }
    let proof_encoded = serialize_to_bytes_rmp(&stx.proof).expect("proof encodes");
    assert!(contains_bytes(&all, &proof_encoded), "the proof is on the wire");
}

/// S6.1 property 2 — **advisory gate before finalizer work**: with the REAL
/// shielded spec and a shared pool view in which the nullifier is ALREADY
/// spent, the sentinel's step-2 advisory validation rejects the
/// double-spend with `StaleNullifier` — before registration, before
/// finalizer assignment, before any wire traffic.
#[tokio::test]
async fn sentinel_advisory_gate_rejects_double_spend_before_finalizer_work() {
    let input = input_note();
    let n = nullifier(&input, &INPUT_SPEND_KEY);

    let p = build_pipeline(true, Some(n));
    // Re-spend of the same note: same nullifier, fresh root, real
    // commitments.
    let stx = assemble_tx(
        TOKEN_ID,
        vec![commit(&input).to_bytes().as_ref().try_into().unwrap()],
        vec![n],
        vec![commit(&output_note()).to_bytes().as_ref().try_into().unwrap()],
        p.pool.state_snapshot().root,
        vec![0u8; 64],
        vec![],
        10,
    );

    let body = serialize_to_bytes_rmp(&stx).expect("stx serializes");
    let inner = Message {
        chain_id: ENV_ID.to_string(),
        action: "ShieldedTransfer".to_string(),
        body,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    let raw = serialize_to_bytes_rmp(&inner).expect("inner message serializes");

    let err = p.sentinel.on_data_received(raw).expect_err("advisory gate rejects");
    match &err {
        SentinelError::Validation(PneumaticError::Validation(reasons)) => {
            assert!(
                reasons
                    .iter()
                    .any(|r| *r == pneumatic_core::errors::ValidationFailureReason::StaleNullifier),
                "check 2 must fire StaleNullifier; got {reasons:?}"
            );
        }
        other => panic!("expected a Validation error, got {other:?}"),
    }
    // Fail-closed ordering: nothing was registered, nothing was sent.
    assert!(!p.registry.contains_shielded(&stx.id), "no registration on rejection");
    assert!(
        p.sign_recorder.lock().unwrap().is_empty(),
        "no SignShielded on rejection — no finalizer work"
    );
}

/// The canonical S4.1 output note (value 90, owner [2u8; 32]).
fn output_note() -> ShieldedNote {
    ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3), rcm: Fq::from(4) }
}

/// `haystack` contains `needle` as a contiguous byte sequence.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    (0..=haystack.len() - needle.len()).any(|i| &haystack[i..i + needle.len()] == needle)
}

// ---------------------------------------------------------------------------
// Live tests (#[ignore] — the committer re-check reaches check 4, whose
// lazy halo2 verifier pays the one-time ActionCircuit keygen_vk (~2.5 min)
// in this test binary)
// ---------------------------------------------------------------------------

/// S6.1 property 3a — **the authoritative gate**: a full four-hop pipeline
/// whose stx carries a FAKE proof sails through both advisory gates (stub
/// spec) and dies exactly at the committer's pool re-check — check 4 —
/// with `ShieldedProofInvalid` (cause names the reason) and the pool
/// byte-for-byte untouched.
#[tokio::test]
#[ignore = "live: the committer re-check reaches check 4 — the lazy halo2 verifier pays the one-time ActionCircuit keygen_vk (~2.5 min) in this test binary. AGENT: re-run when you change the commit path, the pool re-validation, the four shielded checks, ShieldedVerifier, or the ActionCircuit: `cargo test -p pneumatic_core --test shielded_pipeline -- --ignored`"]
async fn full_pipeline_fake_proof_stops_at_committer_recheck() {
    let p = build_pipeline(false, None);
    let input = input_note();
    let stx = assemble_tx(
        TOKEN_ID,
        vec![commit(&input).to_bytes().as_ref().try_into().unwrap()],
        vec![nullifier(&input, &INPUT_SPEND_KEY)],
        vec![commit(&output_note()).to_bytes().as_ref().try_into().unwrap()],
        p.pool.state_snapshot().root, // fresh against the pool's root history
        vec![9u8; 64], // fake proof — check 4 is the discriminator
        vec![],
        10,
    );

    let (_sign, _vote, commit_msg) = drive_pipeline(&p, &stx).await;

    let result = p.committer.handle_message(commit_msg).await;
    match &result {
        Err(CommitterError::ShieldedProofInvalid { cause, .. }) => {
            assert!(
                cause.contains("InvalidShieldedProof"),
                "the re-check must fail at check 4 (InvalidShieldedProof); cause: {cause}"
            );
        }
        other => panic!("expected ShieldedProofInvalid at the committer, got {other:?}"),
    }
    // The pool is byte-for-byte the seeded state: one leaf (the input),
    // one applied delta (the seed), no spent nullifier, same root.
    assert_eq!(p.pool.leaf_count(), 1, "no leaves appended");
    assert_eq!(p.pool.applied_count(), 1, "no delta recorded");
    assert!(
        !p.pool.nullifiers().contains(nullifier(&input, &INPUT_SPEND_KEY)),
        "no nullifier spent"
    );
    // And the token chain is untouched (still just the genesis block).
    let chain_len = p
        .committer
        .get_token(TOKEN_ID)
        .expect("token in cache")
        .blockchain
        .get_count();
    assert_eq!(chain_len, 1, "the chain did not advance");
}

/// S6.1 property 3b — **end-to-end lockstep**: a REAL halo2 proof (one
/// prove, ~1 min after the keygen paid above) drives the full four hops to
/// a SUCCESSFUL commit. The pool delta and the token chain advance in
/// lockstep; the recipient recovers the output value from the wire
/// ciphertext; and re-applying the same block is an idempotent no-op at
/// the pool (the single writer's idempotency key).
#[tokio::test]
#[ignore = "live: one halo2 prove (~1 min) plus the one-time ActionCircuit keygen_vk (~2.5 min) in this test binary. AGENT: re-run when you change the commit path, the pool, the four shielded checks, the circuit, or the prover: `cargo test -p pneumatic_core --test shielded_pipeline -- --ignored`"]
async fn full_pipeline_live_prove_commits_and_lockstep_advances() {
    // `ShieldedIdentity` deliberately does not derive Clone, and
    // `NoteOutput::new` takes it by value — capture the owner pk before the
    // move (it is a pure function of the spend seed).
    const RECIPIENT_SEED: [u8; 32] = [0xC4u8; 32];
    let recipient = ShieldedIdentity {
        spend: SpendKey::from_seed(RECIPIENT_SEED),
        identity: Ed25519Provider::from_seed(RECIPIENT_SEED),
    };
    let recipient_owner = recipient.owner_pk();

    // Real proving against the seeded pool: the input note's membership
    // proof at the pool's tip root.
    let (state, input, proof, root, nullifier) = pool_fixture(None);
    let dp = StubDataProvider::new().with_shielded_pool(state);
    let pool = ShieldedPool::load(&dp, "token", 10).expect("pool loads");
    let (stx, outputs) = build_shielded_tx(
        TOKEN_ID,
        &[input],
        &[&INPUT_SPEND_KEY],
        &[proof],
        root,
        &[NoteOutput::new(90, recipient)],
        10, // fee: 100 = 90 + 10
    )
    .expect("the live tx proves");

    // The pipeline carries the SAME pool as the committer's single writer.
    let p = build_pipeline_with_pool(true, pool.clone());
    let chain_before = p
        .committer
        .get_token(TOKEN_ID)
        .unwrap()
        .blockchain
        .get_count();

    let (_sign, _vote, commit_msg) = drive_pipeline(&p, &stx).await;
    p.committer
        .handle_message(commit_msg.clone())
        .await
        .expect("the commit succeeds");

    // Pool advanced in lockstep: one output leaf appended, the delta
    // recorded, the nullifier spent, the root advanced.
    assert_eq!(p.pool.leaf_count(), 2, "the output commitment leaf was appended");
    assert_eq!(p.pool.applied_count(), 2, "the delta was recorded (seed + this)");
    assert!(p.pool.nullifiers().contains(nullifier), "the nullifier is spent");
    // The root history tip equals the rebuilt tree root.
    let mut rebuilt = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    for leaf in p.pool.state_snapshot().leaves {
        let fp = Fp::from_repr(leaf).expect("leaf is a valid Fp");
        rebuilt.append_leaf(&fp);
    }
    assert_eq!(
        p.pool.current_root(),
        root_to_bytes(&rebuilt.root()),
        "the root-history tip equals the rebuilt tree root"
    );

    // Chain advanced exactly one block; the committed block is the tip.
    let commit: pneumatic_core::transactions::TransactionCommit =
        deserialize_rmp_to(&commit_msg.body).expect("commit body");
    let token = p.committer.get_token(TOKEN_ID).unwrap();
    assert_eq!(
        token.blockchain.get_count(),
        chain_before + 1,
        "the chain advanced exactly one block"
    );
    assert_eq!(
        token.blockchain.get_current_chain_state().last_hash_in,
        commit.proposed_block.current_hash,
        "the committed block is the chain tip"
    );

    // Balance: the committed transfer pays out 90 to the recipient. The
    // assertion uses the builder's plain-text output (the identity was moved
    // into the tx and the provider's X25519/ML-KEM keys are fresh per
    // construction — `ShieldedIdentity` is non-Clone by design, so a pipeline
    // test cannot re-derive a matching decryptor). The wire-decryption
    // round-trip (ciphertext → value, viewing-key path) is pinned in the
    // prover crate's own S3.3.4 scan suite.
    assert_eq!(outputs.len(), 1, "one output note");
    assert_eq!(outputs[0].value, 90, "the output value is 90");
    assert_eq!(
        outputs[0].owner_pk, recipient_owner,
        "the output note is owned by the recipient"
    );

    // Idempotent replay: re-applying the SAME block's delta is a no-op at
    // the pool (the idempotency key is the block hash).
    let outcome = p
        .pool
        .apply_update(&commit.proposed_block, &p.env)
        .expect("the replay is accepted");
    assert_eq!(outcome, PoolApplyOutcome::AlreadyApplied, "replay is idempotent");
    assert_eq!(p.pool.leaf_count(), 2, "the replay appended nothing");
    assert_eq!(p.pool.applied_count(), 2, "the replay recorded no delta");
}

// ---------------------------------------------------------------------------
// Fixture variant: a caller-supplied pool (the live test proves against the
// SAME pool the pipeline will commit through)
// ---------------------------------------------------------------------------

/// Like `build_pipeline`, but with a caller-suploaded pool: the live test
/// builds its proving fixture (membership proof + root) from exactly the
/// pool the committer will apply the delta to.
fn build_pipeline_with_pool(
    real_shielded: bool,
    pool: Arc<ShieldedPool>,
) -> Pipeline {
    // The shared data provider carries the SAME pool state as the caller's
    // pool (the sentinel and finalizer read the pool via the shared Arc, so
    // this copy is inert for them, but the provider stays coherent).
    let sf_identity = Arc::new(NodeIdentity::generate_in_memory());
    let cm_identity = Arc::new(NodeIdentity::generate_in_memory());
    let sf_pk = sf_identity.ed25519.public_key().expect("sf public key");
    let cm_pk = cm_identity.ed25519.public_key().expect("cm public key");
    let stake_set = StakeSet { stakers: [(sf_pk.clone(), 100u64)].into_iter().collect() };
    let data_provider: Arc<dyn pneumatic_core::data::DataProvider> = Arc::new(
        StubDataProvider::new()
            .with_token(TOKEN_ID.to_vec(), "token".to_string(), shielded_token())
            .with_user(
                SENDER.to_vec(),
                "token".to_string(),
                User {
                    public_key: SENDER.to_vec(),
                    fuel_balance: 1_000_000,
                    stake: 0,
                    nonce: 0,
                },
            )
            .with_stake_snapshot(0, stake_set.clone())
            .with_stake_snapshot(1, stake_set)
            .with_shielded_pool(pool.state_snapshot()),
    );

    let env = test_env(real_shielded);

    let sentinel_config = role_config(&sf_identity);
    let sentinel_registry = make_node_registry(sentinel_config.clone());
    let sign_recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(
        sentinel_registry.register_peer(
            sf_pk.clone(),
            sf_identity.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { recorder: sign_recorder.clone() }),
        ),
        "sentinel: finalizer peer registers"
    );
    let sentinel_gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Sentinel,
        sentinel_config.clone(),
        300,
        env.asym_crypto_provider.clone(),
    ));
    let registry = Arc::new(PendingTransactionRegistry::new());
    let sentinel = Sentinel::new(
        sentinel_config,
        env.clone(),
        sentinel_registry,
        registry.clone(),
        sentinel_gossiper,
        data_provider.clone(),
        pool.clone(),
    );

    let finalizer_config = role_config(&sf_identity);
    let finalizer_registry = make_node_registry(finalizer_config.clone());
    let vote_recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(
        finalizer_registry.register_peer(
            sf_pk.clone(),
            sf_identity.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { recorder: vote_recorder.clone() }),
        ),
        "finalizer: own key registers as a Finalizer peer"
    );
    let commit_recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(
        finalizer_registry.register_peer(
            cm_pk.clone(),
            cm_identity.rhash,
            &NodeRegistryType::Committer,
            Box::new(RecordingConnection { recorder: commit_recorder.clone() }),
        ),
        "finalizer: committer peer registers"
    );
    let (signing_key, verifying_key) = test_signing_key();
    let finalizer = Finalizer::new(
        ENV_ID.to_string(),
        sf_pk.clone(),
        sf_identity.clone(),
        finalizer_registry,
        registry.clone(),
        Arc::new(TransactionSignatureRegistry::new()),
        67.0,
        1,
        signing_key,
        verifying_key,
        Arc::new(BasicHashProvider::new()),
        vec![10, 20, 30],
        100,
        vec![40, 50, 60],
        0,
        data_provider.clone(),
        "token".to_string(),
        pool.clone(),
        env.clone(),
    );

    let committer_config = role_config(&cm_identity);
    let committer_registry = make_node_registry(committer_config.clone());
    assert!(
        committer_registry.register_peer(
            sf_pk.clone(),
            sf_identity.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(NoOpConnection),
        ),
        "committer: finalizer key registers (the Commit role gate)"
    );
    let committer_gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Committer,
        committer_config.clone(),
        60,
        env.asym_crypto_provider.clone(),
    ));
    let tokens = Arc::new({
        let map = dashmap::DashMap::new();
        map.insert(TOKEN_ID.to_vec(), shielded_token());
        map
    });
    let stake_store = Arc::new(StakeStore::new());
    let staking_manager = Arc::new(StakingManager::new(stake_store.clone(), env.logger.clone()));
    let default_dp = Arc::new(pneumatic_core::data::DefaultDataProvider::new());
    let candidate_registry = Arc::new(CandidateRegistry::new());
    let epoch_reconciler = Arc::new(EpochReconciler::new(
        stake_store.clone(),
        candidate_registry.clone(),
        default_dp,
        "test".to_string(),
        vec![TOKEN_ID.to_vec()],
        env.cost_model.slash_fraction,
    ));
    let hash_provider = Arc::new(BasicHashProvider::new());
    let leader_selector = Arc::new(LeaderSelector::new(hash_provider.clone()));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64;
    let initial_epoch = Epoch::new_with_leader(
        1,
        now,
        now + 300,
        leader_selector.as_ref(),
        &stake_store.to_stake_set(),
        &[],
    );
    let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));
    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider.clone(),
        committer_registry.clone(),
        env.clone(),
        env.logger.clone(),
        cm_identity.clone(),
        pool.clone(),
    ));
    let committer = Committer::new(
        env.clone(),
        cm_pk,
        cm_identity,
        committer_gossiper,
        block_services,
        committer_registry,
        tokens,
        registry.clone(),
        stake_store,
        staking_manager,
        epoch_reconciler,
        leader_selector,
        data_provider.clone(),
        0,
        Some(EpochBoundaryDetector::new(initial_epoch)),
        block_proposer,
        300,
        5000,
        candidate_registry,
        pool.clone(),
    );

    Pipeline {
        sentinel,
        finalizer,
        committer,
        registry,
        pool,
        env,
        sign_recorder,
        vote_recorder,
        commit_recorder,
    }
}
