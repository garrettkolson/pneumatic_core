//! Live composite S5.4 e2e tests: the full shielded path (all four
//! hops advance the shared pool) and the conflict-rollback chain/pool
//! lockstep scenario.
use super::helpers::*;
use super::super::*;

/// S5.4 composite e2e (LIVE — one halo2 prove): all four roles installed
/// in ONE composite node, and a shielded transfer is driven through all
/// four dispatch hops — Sentinel `Verify` → `SignShielded` → Finalizer
/// `SignShielded` → `ShieldedVote` → Finalizer `ShieldedVote` → `Commit`
/// → Committer `Commit` — with the inter-hop messages relayed by
/// re-dispatching what each role recorded on its peers. The test drives
/// ONLY the four relays: the sentinel does its own registration +
/// finalizer assignment, the finalizer its own build + sign, and the
/// committer materializes its pending entry from the authenticated wire
/// block (H4). The terminal assertion is the pool/chain lockstep: the
/// committer's commit path applied the delta to the SAME
/// `Arc<ShieldedPool>` the roles' views read (S5.4's whole point — one
/// pool, no separate view).
#[tokio::test]
#[ignore = "live halo2 prove (~1 min); run with --ignored"]
async fn live_composite_shielded_e2e_all_four_hops_advance_the_pool() {
    use pneumatic_core::blocks::BlockFactory;
    use pneumatic_core::blocks::FinalityStatus;
    use pneumatic_core::shielded::{
        commit, root_to_bytes, IncrementalMerkleTree, ShieldedNote, DEFAULT_DEPTH,
    };
    use pneumatic_prover::{build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};
    use pasta_curves::pallas::Scalar as Fq;

    // --- the shielded universe: one input note, proven live ------------
    let input_note = ShieldedNote {
        value: 100,
        owner_pk: [1u8; 32],
        rho: Fq::from(1),
        rcm: Fq::from(2),
    };
    let spend_key = [0xABu8; 32];
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root, _) = tree.append(&commit(&input_note));
    let pre_root = root_to_bytes(&root);
    let proof = tree.membership_proof(0);

    // Seed the pool's persisted state with the input note's leaf, so the
    // boot-loaded pool's root history contains the root this tx proves
    // against (within recency) — the same seeding the committer's live
    // pool tests use. `from_state` rebuilds the tree from the delta's
    // leaves, so the seed delta must carry the leaf it produced.
    let leaf =
        root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&input_note)));
    let seed_state = ShieldedPoolState {
        root: pre_root,
        leaf_count: 1,
        leaves: vec![leaf],
        nullifiers: vec![],
        applied: vec![e2e_seed_delta(vec![0xAA; 32], vec![leaf], pre_root)],
    };

    // --- the composite's own identity is the voting finalizer ----------
    // (a split deployment registers its voting finalizer the same way).
    // The key must sit in BOTH the Finalizer bucket (the finalizer's C1
    // gate on SignShielded/ShieldedVote, and the committer's Commit role
    // gate — `Commit` requires the signer's role set to include
    // Finalizer) and the Committer bucket (so the finalizer's outbound
    // Commit has a recorded destination to relay from).
    let cfg = runtime_config_with_shielded_spec(vec![bad_peer()], type_config_floor(0));
    let own_key = cfg.public_key.clone();

    // The token must exist in BOTH the composite's shared token cache
    // (the committer's commit path) and the data provider (the
    // finalizer's `resolve_previous_hash` reads the PROVIDER, not the
    // cache) — the SAME genesis-chained token in both, built BEFORE the
    // provider, so the finalizer's prev-hash and the committer's strict
    // linkage check agree.
    let mut token = make_e2e_token();
    {
        let mut genesis = pneumatic_core::blocks::Block {
            signed_trans: SignedTransaction::test_transaction(),
            token_metadata: std::collections::HashMap::new(),
            previous_hash: vec![42u8; 32],
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        genesis.current_hash =
            BlockFactory::create_hash(&genesis).expect("genesis hashes");
        token.blockchain.add_block(genesis);
    }

    // One in-memory data provider: the seeded pool state, the opt-in
    // self-verified token, the user, and an epoch-1 stake snapshot in
    // which this node's own key is the sole (hence 100%) staker.
    let mut stakers = std::collections::HashMap::new();
    stakers.insert(own_key.clone(), 100u64);
    let dp = Arc::new(E2eDataProvider {
        pool_state: std::sync::Mutex::new(seed_state),
        token: token.clone(),
        user_pk: b"alice".to_vec(),
        stakers,
    });

    let server = build_runtime(cfg, Arc::new(MapStakeProvider::with_default(2000)), dp.clone())
        .expect("composite boots with the seeded pool");
    let pool = server.shielded_pool();
    assert_eq!(pool.leaf_count(), 1, "boot load rebuilt the seeded pool");
    assert_eq!(pool.applied_count(), 1, "the seed delta survived the boot load");

    // Register the composite's own identity as Finalizer + Committer
    // peers with recording connections, so every outbound hop is
    // captured for the relay.
    let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        server.node_registry().register_peer(
            own_key.clone(), [9u8; 16], &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ),
        "own finalizer peer registers"
    );
    assert!(
        server.node_registry().register_peer(
            own_key.clone(), [9u8; 16], &NodeRegistryType::Committer,
            Box::new(RecordingConnection { recorder: recorder.clone() }),
        ),
        "own committer peer registers"
    );

    // Prove the live transfer against the pool's seeded root: 100 = 90 + 10.
    let recipient = ShieldedIdentity {
        spend: SpendKey::from_seed([2u8; 32]),
        identity: pneumatic_core::crypto::Ed25519Provider::generate(),
    };
    let (stx, _) = build_shielded_tx(
        &[1u8], &[input_note], &[&spend_key], &[proof],
        pre_root, &[NoteOutput::new(90, recipient)], 10,
    )
    .expect("the real prove must succeed");
    let nullifier = stx.nullifiers[0];

    // Install the SAME genesis-chained token in the composite's shared
    // token cache (the committer's commit path).
    {
        let tokens = server.tokens();
        tokens.entry(vec![1]).or_insert(token);
    }
    let tokens_before = server
        .tokens()
        .get(&vec![1])
        .map(|t| t.blockchain.get_count())
        .unwrap_or(0);

    // --- hop 1: the sentinel's `Verify` (inner ShieldedTransfer) -------
    // The sentinel performs its own advisory gate, token gates, the
    // parallel `register_shielded`, and the deterministic finalizer
    // assignment, and records the outbound SignShielded — nothing else
    // to pre-stage (the committer materializes its pending entry from
    // the authenticated wire block, H4).
    let verify_msg = Message {
        chain_id: "env".to_string(),
        action: "Verify".to_string(),
        body: serialize_to_bytes_rmp(
            &Message {
                chain_id: "env".to_string(),
                action: "ShieldedTransfer".to_string(),
                body: serialize_to_bytes_rmp(&stx).expect("stx serializes"),
                signature: vec![],
                public_key: vec![],
                stake_set: None,
            },
        )
        .expect("inner message serializes"),
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    server
        .dispatch(verify_msg)
        .await
        .expect("hop 1: the sentinel accepts the live transfer");

    // Hop 2: relay the sentinel's recorded SignShielded to the finalizer.
    let sign = next_recorded(&recorder, "SignShielded").await;
    server.dispatch(sign).await.expect("hop 2: the finalizer signs the vote");

    // Hop 3: relay the finalizer's recorded ShieldedVote.
    let vote = next_recorded(&recorder, "ShieldedVote").await;
    server.dispatch(vote).await.expect("hop 3: quorum finalizes the transfer");

    // Hop 4: relay the finalizer's recorded Commit to the committer.
    let commit_msg = next_recorded(&recorder, "Commit").await;
    server
        .dispatch(commit_msg.clone())
        .await
        .expect("hop 4: the committer commits the shielded block");
    let commit: TransactionCommit =
        deserialize_rmp_to(&commit_msg.body).expect("commit body deserializes");

    // --- the lockstep assertion: ONE pool advanced, the chain followed -
    assert_eq!(pool.leaf_count(), 2, "the output commitment leaf was appended");
    assert_eq!(pool.applied_count(), 2, "seed delta + this commit's delta");
    assert!(pool.nullifiers().contains(nullifier), "the spend is recorded");
    assert_eq!(
        pool.current_root(),
        pool_tree_root(&pool),
        "the root-history tip equals the rebuilt tree root"
    );
    let tip = server
        .tokens()
        .get(&vec![1])
        .unwrap()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;
    assert_eq!(
        tip,
        commit.proposed_block.current_hash,
        "the committed block is the chain tip"
    );
    let count = server.tokens().get(&vec![1]).unwrap().blockchain.get_count();
    assert_eq!(count, tokens_before + 1, "chain advanced exactly one block");
}

/// S5.4 conflict check (LIVE — two halo2 proves): a shielded block that
/// LOSES its tip conflict is rolled back lockstep — the loser's pool
/// delta is reverted (leaves, nullifiers, root history) through the
/// COMPOSITE's dispatch path (dispatcher → committer plugin →
/// `commit_block` rollback branch → pool `revert_update`), proving the
/// machinery needs no epoch-logic changes: the lockstep the committer
/// owns works unchanged inside the composite. The winner's delta stays
/// applied. Finalizer-free by design: both Commits are hand-signed by
/// two registered finalizer identities with UNEQUAL stakes (100 vs 200 —
/// an equal-stake tie would fail closed) — the committer's conflict
/// resolution is the unit under test, not the finalizer's quorum math.
#[tokio::test]
#[ignore = "live halo2 prove x2 (~2 min); run with --ignored"]
async fn live_composite_conflict_rollback_lockstep() {
    use pneumatic_core::blocks::BlockFactory;
    use pneumatic_core::blocks::FinalityStatus;
    use pneumatic_core::crypto::AsymCryptoProvider;
    use pneumatic_core::shielded::{
        commit, root_to_bytes, IncrementalMerkleTree, ShieldedNote, DEFAULT_DEPTH,
    };
    use pneumatic_prover::{build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};
    use pasta_curves::pallas::Scalar as Fq;

    // TWO input notes in ONE tree: the sibling proposals A and B each
    // prove against the FINAL root (the pool's current root — the root
    // history both recency checks run against).
    let note_a = ShieldedNote {
        value: 100,
        owner_pk: [1u8; 32],
        rho: Fq::from(1),
        rcm: Fq::from(2),
    };
    let note_b = ShieldedNote {
        value: 200,
        owner_pk: [2u8; 32],
        rho: Fq::from(3),
        rcm: Fq::from(4),
    };
    let spend_a = [0xA1u8; 32];
    let spend_b = [0xB2u8; 32];
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root_a, _) = tree.append(&commit(&note_a));
    let (root_b, proof_b) = tree.append(&commit(&note_b));
    let root_a = root_to_bytes(&root_a);
    let root_b = root_to_bytes(&root_b);
    let leaf_a = root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&note_a)));
    let leaf_b = root_to_bytes(&IncrementalMerkleTree::commitment_to_leaf(&commit(&note_b)));
    // note_a's membership re-derived against the FINAL root.
    let proof_a = tree.membership_proof(0);

    // Seed the pool with BOTH leaves in two prior deltas (the final root
    // as the tip — the pool's boot state).
    let seed_state = ShieldedPoolState {
        root: root_b,
        leaf_count: 2,
        leaves: vec![leaf_a, leaf_b],
        nullifiers: vec![],
        applied: vec![
            e2e_seed_delta(vec![0xAA; 32], vec![leaf_a], root_a),
            e2e_seed_delta(vec![0xAB; 32], vec![leaf_b], root_b),
        ],
    };

    // Two distinct finalizer identities: the conflicting proposers.
    let identity_a = NodeIdentity::generate_in_memory();
    let identity_b = NodeIdentity::generate_in_memory();
    let key_a = identity_a.ed25519.public_key().expect("A pubkey");
    let key_b = identity_b.ed25519.public_key().expect("B pubkey");

    // Committer-only composite: the unit under test is the committer's
    // commit path (the S5.4 pool swap) — no sentinel/finalizer arms.
    let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Committer));
    let mut stakers = std::collections::HashMap::new();
    stakers.insert(key_a.clone(), 100u64);
    stakers.insert(key_b.clone(), 200u64);
    let dp = Arc::new(E2eDataProvider {
        pool_state: std::sync::Mutex::new(seed_state),
        token: make_e2e_token(),
        user_pk: b"alice".to_vec(),
        stakers,
    });
    let server = build_runtime(cfg, Arc::new(MapStakeProvider::with_default(2000)), dp)
        .expect("composite boots with the seeded pool");
    let pool = server.shielded_pool();
    assert_eq!(pool.leaf_count(), 2, "boot load rebuilt the seeded pool");
    assert_eq!(pool.applied_count(), 2, "both seed deltas survived the boot load");

    // Prove BOTH sibling spends live, against the shared FINAL root:
    // A: 100 = 90 + 10; B: 200 = 190 + 10.
    let recipient_a = ShieldedIdentity {
        spend: SpendKey::from_seed([2u8; 32]),
        identity: pneumatic_core::crypto::Ed25519Provider::generate(),
    };
    let recipient_b = ShieldedIdentity {
        spend: SpendKey::from_seed([3u8; 32]),
        identity: pneumatic_core::crypto::Ed25519Provider::generate(),
    };
    let (stx_a, _) = build_shielded_tx(
        &[1u8], &[note_a], &[&spend_a], &[proof_a],
        root_b, &[NoteOutput::new(90, recipient_a)], 10,
    )
    .expect("prove A");
    let (stx_b, _) = build_shielded_tx(
        &[1u8], &[note_b], &[&spend_b], &[proof_b],
        root_b, &[NoteOutput::new(190, recipient_b)], 10,
    )
    .expect("prove B");
    let null_a = stx_a.nullifiers[0];
    let null_b = stx_b.nullifiers[0];

    // Bootstrap the token (shared cache) with a genesis.
    {
        let tokens = server.tokens();
        let mut t = tokens.entry(vec![1]).or_insert(make_e2e_token());
        let mut genesis = pneumatic_core::blocks::Block {
            signed_trans: SignedTransaction::test_transaction(),
            token_metadata: std::collections::HashMap::new(),
            previous_hash: vec![42u8; 32],
            timestamp: 0,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };
        genesis.current_hash = BlockFactory::create_hash(&genesis).expect("genesis hashes");
        t.blockchain.add_block(genesis);
    }

    // Register BOTH proposers as Finalizer peers (recording connections
    // — nothing is actually sent), so the committer's Commit auth
    // resolves their role sets and their stake from the snapshot.
    let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
    for (key, rhash) in
        [(key_a.clone(), [0xA0u8; 16]), (key_b.clone(), [0xB0u8; 16])]
    {
        assert!(
            server.node_registry().register_peer(
                key, rhash, &NodeRegistryType::Finalizer,
                Box::new(RecordingConnection { recorder: recorder.clone() }),
            ),
            "proposer peer registers"
        );
    }

    // H12 shielded pairing for both siblings: each wire block's stx must
    // hash-match a registered shielded entry (no plain entries — the
    // committer materializes them from the wire block, H4).
    let registry = server.pending_registry();
    registry.register_shielded(&stx_a).expect("A's payload registers");
    registry.register_shielded(&stx_b).expect("B's payload registers");

    // A commits first: the block is chained off the tip captured NOW
    // (the genesis hash) and signed by proposer A's registered
    // finalizer identity.
    let genesis_tip = server
        .tokens()
        .get(&vec![1])
        .map(|t| {
            let s = t.blockchain.get_current_chain_state();
            if s.last_hash_in.is_empty() {
                Vec::new()
            } else {
                s.last_hash_in
            }
        })
        .unwrap_or_default();
    let block_a = make_e2e_shielded_block(&stx_a, genesis_tip.clone());
    let commit_a = e2e_commit(&stx_a, &block_a);
    server
        .dispatch(e2e_commit_message(&commit_a, &identity_a))
        .await
        .expect("A commits and becomes the tip");
    assert!(pool.nullifiers().contains(null_a), "A's delta is applied");

    // B (higher stake) commits at the SAME chain position — chained off
    // the genesis tip captured BEFORE A committed, so the conflict is on
    // the position, not the prev hash. B wins; A is rolled back
    // lockstep through the composite's commit path.
    let block_b = make_e2e_shielded_block(&stx_b, genesis_tip);
    let commit_b = e2e_commit(&stx_b, &block_b);
    server
        .dispatch(e2e_commit_message(&commit_b, &identity_b))
        .await
        .expect("B wins the conflict and commits");

    // Chain: A is gone, B is the sole block at the position.
    let tip = server
        .tokens()
        .get(&vec![1])
        .unwrap()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;
    assert_eq!(tip, commit_b.proposed_block.current_hash, "B is the tip");
    assert_eq!(
        server.tokens().get(&vec![1]).unwrap().blockchain.get_count(),
        2,
        "genesis + B: A rolled back"
    );

    // Pool: LOCKSTEP — A's delta reverted, B's delta intact.
    assert_eq!(
        pool.leaf_count(),
        3,
        "two seed leaves + B's leaf (A's leaf reverted)"
    );
    assert!(
        !pool.nullifiers().contains(null_a),
        "A's nullifier is unmarked (delta reverted)"
    );
    assert!(
        pool.nullifiers().contains(null_b),
        "B's nullifier is marked"
    );
    assert_eq!(
        pool.applied_count(),
        3,
        "two seed deltas + B's delta (A's delta reverted)"
    );
    assert_eq!(
        pool.current_root(),
        pool_tree_root(&pool),
        "post-rollback root history is coherent"
    );
}

// ===========================================================================
// P10 (executor plan, Phase 10): end-to-end contract execution over the
// STANDARD pipeline in the composite — Sentinel `Verify` → Executor
// `Preload` → Finalizer `Sign` → Committer `Commit` — for every Tier-1
// engine path:
//
//   (a) standard transfer tx        (TransferEngine)
//   (b) `Spec` contract tx          (SpecEngine, rmp-AST bytecode)
//   (c) `Wasm` contract tx          (WasmEngine — pure + stateful W3 storage)
//   (d) deployment tx               (ADR-015 — Spec and Wasm)
//   (e) cross-contract call tx      (ADR-016 Model X, Spec → Spec)
//
// The terminal assertion in every case: the committed block's
// `signed_trans.transaction.result_hash` equals `hash(engine_output)`
// computed by running the engine DIRECTLY (no pipeline involvement) —
// the end-to-end determinism property the plan requires.
//
// These tests also exercise the composite shared-registry path end to
// end: one `PendingTransactionRegistry` shared by all four roles, so the
// finalizer's `Committed` booking and the stamped `result_hash` must be
// reconciled with the committer's commit (see the P10 fixes in
// finalizer/signing.rs, finalizer/finalizing.rs, committer/committing.rs).
// ===========================================================================

use pneumatic_core::contracts::{
    deploy_contract, CallContext, ContractEngine, ContractEngineRegistry, ContractError,
    DeployParams, ExecutionInput, InstructionProgram, Op, PinnedTarget, SnapshotRef,
    TargetStateProvider, TransferDelta, WasmEngine, WasmResult,
};
use pneumatic_core::crypto::{AsymCryptoProvider, Ed25519Provider};
use pneumatic_core::data::DataError;
use pneumatic_core::tokens::SmartContract;
use pneumatic_core::transactions::Transaction;

/// The environment spec for the contract pipeline. `load_from_spec` only
/// validates the built-in Tier-1 engine list (`["Transfer", "Spec"]`), so
/// `Wasm` (opt-in) is registered on the env's `ContractEngineRegistry` AFTER
/// load in `contract_runtime_config` (ADR-011).
const CONTRACT_SPEC: &str = r#"{
    "environment_id": "test_env",
    "environment_name": "Contract Pipeline E2E Environment",
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
    "contract_engines": ["Transfer", "Spec"]
}"#;

/// In-memory multi-token / multi-user `DataProvider` for the contract
/// pipeline. Unlike `E2eDataProvider` (single token, single user) it serves
/// a fixed set of tokens + users, persists `save_token` / `save_user`
/// (the committer's gas deduction and deploy apply write back through
/// here), and answers every stake-snapshot read with the fixed staker set.
#[derive(Clone)]
struct ContractE2eProvider {
    tokens: Arc<DashMap<Vec<u8>, Token>>,
    users: Arc<DashMap<Vec<u8>, User>>,
    stakers: Arc<std::sync::Mutex<HashMap<Vec<u8>, u64>>>,
}

impl ContractE2eProvider {
    fn new(
        tokens: Vec<(Vec<u8>, Token)>,
        users: Vec<(Vec<u8>, User)>,
        stakers: Vec<(Vec<u8>, u64)>,
    ) -> Self {
        ContractE2eProvider {
            tokens: Arc::new(DashMap::from_iter(tokens)),
            users: Arc::new(DashMap::from_iter(users)),
            stakers: Arc::new(std::sync::Mutex::new(HashMap::from_iter(stakers))),
        }
    }
}

impl DataProvider for ContractE2eProvider {
    fn get_token(&self, key: &Vec<u8>, _partition_id: &str) -> Result<Token, DataError> {
        self.tokens
            .get(key)
            .map(|t| t.clone())
            .ok_or(DataError::DataNotFound)
    }
    fn save_token(&self, key: &Vec<u8>, token: Token, _partition_id: &str) -> Result<(), DataError> {
        self.tokens.insert(key.clone(), token);
        Ok(())
    }
    fn get_user(&self, key: &Vec<u8>, _partition_id: &str) -> Result<User, DataError> {
        self.users
            .get(key)
            .map(|u| u.clone())
            .ok_or(DataError::DataNotFound)
    }
    fn save_user(&self, key: &Vec<u8>, user: User, _partition_id: &str) -> Result<(), DataError> {
        self.users.insert(key.clone(), user);
        Ok(())
    }
    fn get_stake_snapshot(
        &self,
        _epoch: u64,
        _partition_id: &str,
    ) -> Result<StakeSet, DataError> {
        Ok(StakeSet {
            stakers: self.stakers.lock().unwrap().clone(),
        })
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
}

/// A `Config` whose `test_env` declares the three contract engines and
/// whose `DeployContract` spec is wired to the TEST data provider:
/// `load_from_spec` auto-registers the deploy spec against a lazy
/// `DefaultDataProvider` (an unreachable TCP/UDS client), so without this
/// re-registration the deploy nonce check can never see our users and
/// fails closed. `register` overwrites by name.
fn contract_runtime_config(
    bootstrap: Vec<BootstrapPeer>,
    type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
    provider: Arc<dyn DataProvider>,
) -> Arc<Config> {
    let spec = serde_json::from_str::<EnvironmentMetadataSpec>(CONTRACT_SPEC)
        .expect("valid contract environment spec");
    let mut env = EnvironmentMetadata::load_from_spec(spec).expect("contract spec loads");
    // `Wasm` is opt-in: `load_from_spec` only validates the built-in Tier-1
    // list (`Transfer`, `Spec`), so the env registry gains the Wasm engine
    // AFTER load (ADR-011 opt-in path).
    env.contract_engines.register(Arc::new(WasmEngine));
    let mut specs = ValidationSpecRegistry::new();
    specs.register_defaults();
    specs.register_deploy(env.contract_engines.clone(), provider);
    env.transaction_validation_specs = Arc::new(specs);
    let registry = Arc::new(DashMap::new());
    registry.insert(env.environment_id.clone(), env);
    let mut cfg = Config::new_for_testing("test_env".into(), registry, type_configs);
    cfg.bootstrap_peers = bootstrap;
    Arc::new(cfg)
}

// --- fixtures -------------------------------------------------------------

/// A fresh genesis-chained pipeline token: `Executed` block validation
/// (registered in the env's block-spec registry by `load_from_spec`),
/// non-self-verified (the standard pipeline applies).
fn pipeline_token(id: Vec<u8>) -> Token {
    let mut t = Token::new();
    t.id = id;
    t.block_validation_spec_name = "Executed".to_string();
    let mut block = pneumatic_core::blocks::Block {
        signed_trans: pneumatic_core::transactions::SignedTransaction::test_transaction(),
        token_metadata: HashMap::new(),
        previous_hash: vec![],
        current_hash: vec![],
        timestamp: 0,
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };
    block.current_hash = BlockFactory::create_hash(&block).expect("genesis hashes");
    t.blockchain.add_block(block);
    t
}

/// A `SmartContract` asset with the given engine bytecode.
fn contract_asset(name: &str, bytecode: Vec<u8>) -> SmartContract {
    SmartContract {
        name: name.to_string(),
        bytecode,
        version: "1".to_string(),
        storage: Default::default(),
        owners: vec![],
        threshold: 0,
    }
}

/// A plain (non-contract) pipeline token: a `SmartContract` asset (the
/// executor's contract path requires one) but no `contract_engine`
/// metadata, so `select_engine` falls through to `"Transfer"`.
fn transfer_token(id: Vec<u8>) -> Token {
    let mut t = pipeline_token(id);
    t.set_asset(&contract_asset("plain-token", vec![])).expect("asset");
    t
}

/// A contract pipeline token (`token_type = "contract"`) naming `engine`.
fn contract_token(id: Vec<u8>, engine: &str, name: &str, bytecode: Vec<u8>) -> Token {
    let mut t = pipeline_token(id);
    t.set_metadata("token_type".into(), "contract".into());
    t.set_metadata("contract_engine".into(), engine.to_string());
    t.set_asset(&contract_asset(name, bytecode)).expect("asset");
    t
}

/// The sender account: a deterministic Ed25519 key (the `tx.sender`
/// public key + the `sender_signature` + the inner "Process" envelope all
/// derive from it — the sentinel's C3 gate requires
/// `message.public_key == tx.sender`).
fn pipeline_sender() -> (Ed25519Provider, Vec<u8>) {
    let account = Ed25519Provider::from_seed([0x42u8; 32]);
    let pk = account.public_key().expect("ed25519 public key");
    (account, pk)
}

/// A sender-signed pipeline transaction (C3: `sender_signature` over
/// `canonical_signature_bytes`, `sequence_number == 1` matching the
/// sender's `User.nonce`).
fn pipeline_tx(
    account: &Ed25519Provider,
    sender: &Vec<u8>,
    id: &str,
    action: &str,
    token_id: &Vec<u8>,
    receiver: &Vec<u8>,
    amount: Option<u64>,
    payload: Vec<u8>,
) -> Transaction {
    let mut tx = Transaction {
        id: id.to_string(),
        action: action.to_string(),
        token_id: token_id.clone(),
        bid: None,
        sequence_number: 1,
        sender: sender.clone(),
        receiver: receiver.clone(),
        amount,
        timestamp: 1_700_000_000,
        result_hash: vec![],
        sender_signature: vec![],
        payload,
        gas_limit: 0,
        result_data: vec![],
    };
    let canon = tx.canonical_signature_bytes().expect("canonical signature bytes");
    tx.sender_signature = account.sign_data(&canon).expect("sender signs the tx");
    tx
}

/// The sender's `User` record: ample fuel for the committer's gas
/// deduction and `nonce == 1` (the deploy spec's replay gate).
fn pipeline_user(pk: &Vec<u8>) -> User {
    User {
        public_key: pk.clone(),
        fuel_balance: 1_000_000,
        stake: 0,
        nonce: 1,
    }
}

/// One recorder per role bucket — the composite registers the node's own
/// key in all four buckets, and the relay re-dispatches what each bucket
/// recorded.
struct PipelineRecorders {
    sentinels: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    executors: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    finalizers: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    committers: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

fn fresh_recorders() -> PipelineRecorders {
    let r = || Arc::new(std::sync::Mutex::new(Vec::new()));
    PipelineRecorders {
        sentinels: r(),
        executors: r(),
        finalizers: r(),
        committers: r(),
    }
}

/// Drive one transaction through the full standard pipeline:
/// `Verify` (inner `Process`) → `Preload` (executor) → `Preload` + `Sign`
/// (finalizer) → `Commit` (committer). Every relay polls the recorder
/// with a 5 s deadline, so a silent pipeline failure (sentinel validation
/// drop, executor failure, finalizer gate) panics with a clear message
/// instead of a vague assertion.
async fn drive_pipeline_to_commit(
    server: &NodeServer,
    rec: &PipelineRecorders,
    tx: &Transaction,
    account: &Ed25519Provider,
    sender_pk: &Vec<u8>,
) {
    // Hop 1: the sentinel's "Verify" — body is the signed inner "Process".
    let inner_body = serialize_to_bytes_rmp(tx).expect("tx serializes");
    let inner = Message {
        signature: account.sign_data(&inner_body).expect("sender signs the process body"),
        public_key: sender_pk.clone(),
        chain_id: "test_env".to_string(),
        action: "Process".to_string(),
        body: inner_body,
        stake_set: None,
    };
    let outer = msg("Verify");
    let outer = Message {
        chain_id: outer.chain_id,
        action: outer.action,
        body: serialize_to_bytes_rmp(&inner).expect("inner message serializes"),
        signature: outer.signature,
        public_key: outer.public_key,
        stake_set: None,
    };
    server.dispatch(outer).await.expect("sentinel Verify dispatch");

    // Hop 2: the executor's "Preload" (the sentinel's broadcast).
    let exec_preload = next_recorded(&rec.executors, "Preload").await;
    server
        .dispatch(exec_preload)
        .await
        .expect("executor Preload dispatch");

    // Hop 3: the finalizer's own preload — the executor's stamped hop, now
    // riding "PreloadForFinalizer" so it reaches handle_preload instead of
    // looping back through the executor's dispatcher arm
    // (fact-composite-fanout-role-collision) — then the "Sign" vote.
    let _ = next_recorded(&rec.finalizers, "PreloadForFinalizer").await;
    let fin_sign = next_recorded(&rec.finalizers, "Sign").await;
    server
        .dispatch(fin_sign)
        .await
        .expect("finalizer Sign dispatch");

    // Hop 4: the committer's "Commit" (the finalizer's optimistic block).
    let commit = next_recorded(&rec.committers, "Commit").await;
    server
        .dispatch(commit)
        .await
        .expect("committer Commit dispatch");
}


/// The P10 terminal assertion: the token's chain grew by exactly one
/// block and that block's `result_hash` equals `hash(expected)`, where
/// `expected` is the engine output computed OUTSIDE the pipeline.
fn assert_committed_result_hash(
    server: &NodeServer,
    token_id: &Vec<u8>,
    expected_result_data: &[u8],
    label: &str,
) {
    let hash = BasicHashProvider::new();
    let expected = hash.hash(expected_result_data);
    let token_cache = server.tokens();
    let token = token_cache
        .get(token_id)
        .unwrap_or_else(|| panic!("{label}: token {:02x?} missing from the committer cache", token_id));
    assert_eq!(
        token.blockchain.get_count(),
        2,
        "{label}: chain must hold genesis + exactly one committed block"
    );
    let block = token
        .blockchain
        .get_block_at(1)
        .expect("{label}: committed block present");
    assert_eq!(
        block.signed_trans.transaction.result_hash,
        expected,
        "{label}: committed block result_hash must equal hash(engine_output)"
    );
}

/// A composite with all four roles installed (one shared registry), the
/// node's own key recorded in every role bucket, and the given tokens
/// seeded in BOTH the provider and the committer's token cache (the
/// finalizer's `resolve_previous_hash` reads the provider; the committer's
/// `commit_block` reads the cache — the same objects in both).
async fn contract_composite(
    provider: Arc<ContractE2eProvider>,
    tokens: Vec<(Vec<u8>, Token)>,
) -> (
    Arc<Config>,
    NodeServer,
    PipelineRecorders,
    Vec<u8>,
    Arc<ContractE2eProvider>,
) {
    let provider_dyn: Arc<dyn DataProvider> = provider.clone();
    let cfg = contract_runtime_config(
        vec![bad_peer()],
        type_config_floor(0),
        provider_dyn.clone(),
    );
    let own_key = cfg.public_key.clone();
    let stake = Arc::new(MapStakeProvider::with_default(2_000));
    let server = build_runtime(cfg.clone(), stake, provider_dyn.clone())
        .expect("composite boots");

    let rec = fresh_recorders();
    let reg = server.node_registry();
    let c = |r: Arc<std::sync::Mutex<Vec<Vec<u8>>>>| {
        Box::new(RecordingConnection { recorder: r }) as Box<dyn Connection>
    };
    assert!(
        reg.register_peer(own_key.clone(), [1u8; 16], &NodeRegistryType::Sentinel, c(rec.sentinels.clone())),
        "sentinel peer registers"
    );
    assert!(
        reg.register_peer(own_key.clone(), [2u8; 16], &NodeRegistryType::Executor, c(rec.executors.clone())),
        "executor peer registers"
    );
    assert!(
        reg.register_peer(own_key.clone(), [3u8; 16], &NodeRegistryType::Finalizer, c(rec.finalizers.clone())),
        "finalizer peer registers"
    );
    assert!(
        reg.register_peer(own_key.clone(), [4u8; 16], &NodeRegistryType::Committer, c(rec.committers.clone())),
        "committer peer registers"
    );

    for (id, token) in tokens {
        provider.tokens.insert(id.clone(), token.clone());
        server.tokens().insert(id, token);
    }
    // Composite invariant: every token the pipeline can touch must be in the
    // committer's shared cache as well — `BlockServices::commit_block` reads
    // the cache, never the data provider.
    for entry in provider.tokens.iter() {
        server
            .tokens()
            .entry(entry.key().clone())
            .or_insert_with(|| entry.value().clone());
    }

    (cfg, server, rec, own_key, provider)
}

// --- (a) standard transfer -------------------------------------------------

#[tokio::test]
async fn pipeline_transfer_tx_commits_with_independent_transfer_hash() {
    use pneumatic_core::contracts::{ContractEngine, ContractEngineRegistry, TransferDelta};

    let (account, sender_pk) = pipeline_sender();
    let token = transfer_token(vec![0x0A]);
    let token_id = token.id.clone();
    let provider = Arc::new(ContractE2eProvider::new(
        vec![(token_id.clone(), token.clone())],
        vec![(sender_pk.clone(), pipeline_user(&sender_pk))],
        vec![(own_staker_key(), 2_000)],
    ));

    let (_cfg, server, rec, _own_key, _provider) =
        contract_composite(provider, vec![]).await;

    let tx = pipeline_tx(
        &account,
        &sender_pk,
        "e2e_transfer_1",
        "Transfer",
        &token_id,
        &vec![0x77],
        Some(100),
        vec![],
    );

    // Independent expected output: run the TransferEngine directly.
    let contract = token
        .get_asset::<SmartContract>()
        .expect("transfer token carries a contract asset");
    let user = pipeline_user(&sender_pk);
    let engines = Arc::new(ContractEngineRegistry::new());
    engines.register_defaults();
    let engine = engines.get("Transfer").expect("Transfer registered");
    let out = engine
        .execute(&ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 0,
            storage: contract.storage.clone(),
            call_ctx: None,
        })
        .expect("transfer engine executes");
    let expected = out.result_data.clone();
    // Cross-check the canonical delta shape.
    let delta: TransferDelta =
        deserialize_rmp_to(&expected).expect("transfer result is a TransferDelta");
    assert_eq!(delta.amount, 100);

    drive_pipeline_to_commit(&server, &rec, &tx, &account, &sender_pk).await;
    assert_committed_result_hash(&server, &token_id, &expected, "transfer");
}

fn own_staker_key() -> Vec<u8> {
    // The composite's own key is the staker; the stake snapshot is only
    // read for stake-gated role checks (all floors are 0), so the key's
    // value is unimportant — it just must be non-empty.
    vec![0xE0, 0x01]
}

// --- (b) Spec contract ------------------------------------------------------

#[tokio::test]
async fn pipeline_spec_contract_tx_commits_with_independent_spec_hash() {
    use pneumatic_core::contracts::{ContractEngine, ContractEngineRegistry};

    let (account, sender_pk) = pipeline_sender();
    // 7 + 35 → 42, emitted as an 8-byte LE u64.
    let program = pneumatic_core::contracts::InstructionProgram {
        version: 1,
        ops: vec![
            pneumatic_core::contracts::Op::LoadConst(7),
            pneumatic_core::contracts::Op::LoadConst(35),
            pneumatic_core::contracts::Op::Add,
            pneumatic_core::contracts::Op::Emit,
        ],
    };
    let bytecode = serialize_to_bytes_rmp(&program).expect("spec program serializes");
    let token = contract_token(vec![0x0B], "Spec", "spec-adder", bytecode);
    let token_id = token.id.clone();
    let provider = Arc::new(ContractE2eProvider::new(
        vec![(token_id.clone(), token.clone())],
        vec![(sender_pk.clone(), pipeline_user(&sender_pk))],
        vec![(own_staker_key(), 2_000)],
    ));

    let (_cfg, server, rec, _own_key, _provider) =
        contract_composite(provider, vec![]).await;

    let tx = pipeline_tx(
        &account,
        &sender_pk,
        "e2e_spec_1",
        "ContractCall",
        &token_id,
        &vec![],
        Some(10),
        vec![],
    );

    // Independent expected output: run the SpecEngine directly.
    let contract = token
        .get_asset::<SmartContract>()
        .expect("spec token carries a contract asset");
    let user = pipeline_user(&sender_pk);
    let engines = Arc::new(ContractEngineRegistry::new());
    engines.register_defaults();
    let engine = engines.get("Spec").expect("Spec registered");
    let out = engine
        .execute(&ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 0,
            storage: contract.storage.clone(),
            call_ctx: None,
        })
        .expect("spec engine executes");
    assert_eq!(out.result_data, 42u64.to_le_bytes().to_vec());

    drive_pipeline_to_commit(&server, &rec, &tx, &account, &sender_pk).await;
    assert_committed_result_hash(&server, &token_id, &out.result_data, "spec");
}

// --- (c) Wasm contract (pure + stateful W3) ---------------------------------

#[tokio::test]
async fn pipeline_wasm_sum_tx_commits_with_independent_wasm_hash() {
    use pneumatic_core::contracts::{ContractEngine, ContractEngineRegistry, WasmEngine};

    let (account, sender_pk) = pipeline_sender();
    let wasm_bytes = include_bytes!("../../../../src/contracts/wasm_fixtures/wasm_sum.wasm");
    let token = contract_token(vec![0x0C], "Wasm", "wasm-sum", wasm_bytes.to_vec());
    let token_id = token.id.clone();
    let provider = Arc::new(ContractE2eProvider::new(
        vec![(token_id.clone(), token.clone())],
        vec![(sender_pk.clone(), pipeline_user(&sender_pk))],
        vec![(own_staker_key(), 2_000)],
    ));

    let (_cfg, server, rec, _own_key, _provider) =
        contract_composite(provider, vec![]).await;

    let tx = pipeline_tx(
        &account,
        &sender_pk,
        "e2e_wasm_sum_1",
        "ContractCall",
        &token_id,
        &vec![],
        Some(10),
        vec![],
    );

    // Independent expected output: run the WasmEngine directly (the module
    // sums the canonical-input bytes and emits a LE u32; `result_data` is
    // the canonical `WasmResult` envelope).
    let contract = token
        .get_asset::<SmartContract>()
        .expect("wasm token carries a contract asset");
    let user = pipeline_user(&sender_pk);
    let engines = Arc::new(ContractEngineRegistry::new());
    engines.register_defaults();
    engines.register(Arc::new(WasmEngine));
    let engine = engines.get("Wasm").expect("Wasm registered");
    let out = engine
        .execute(&ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 0,
            storage: contract.storage.clone(),
            call_ctx: None,
        })
        .expect("wasm engine executes");
    let envelope: pneumatic_core::contracts::WasmResult =
        deserialize_rmp_to(&out.result_data).expect("wasm result envelope");
    assert_eq!(envelope.module_output.len(), 4, "sum module emits a LE u32");
    assert!(envelope.storage_delta.is_empty());

    drive_pipeline_to_commit(&server, &rec, &tx, &account, &sender_pk).await;
    assert_committed_result_hash(&server, &token_id, &out.result_data, "wasm-sum");
}

#[tokio::test]
async fn pipeline_wasm_storage_tx_commits_with_independent_wasm_hash() {
    use pneumatic_core::contracts::{ContractEngine, ContractEngineRegistry, WasmEngine};

    let (account, sender_pk) = pipeline_sender();
    let wasm_bytes =
        include_bytes!("../../../../src/contracts/wasm_fixtures/wasm_storage.wasm");
    let token = contract_token(vec![0x0D], "Wasm", "wasm-storage", wasm_bytes.to_vec());
    let token_id = token.id.clone();
    let provider = Arc::new(ContractE2eProvider::new(
        vec![(token_id.clone(), token.clone())],
        vec![(sender_pk.clone(), pipeline_user(&sender_pk))],
        vec![(own_staker_key(), 2_000)],
    ));

    let (_cfg, server, rec, _own_key, _provider) =
        contract_composite(provider.clone(), vec![]).await;

    let tx = pipeline_tx(
        &account,
        &sender_pk,
        "e2e_wasm_storage_1",
        "ContractCall",
        &token_id,
        &vec![],
        Some(10),
        vec![],
    );

    // Independent expected output: run the WasmEngine directly (the W3
    // stateful module: store/load/rewrite/delete — the envelope carries the
    // tombstone delta).
    let contract = token
        .get_asset::<SmartContract>()
        .expect("wasm token carries a contract asset");
    let user = pipeline_user(&sender_pk);
    let engines = Arc::new(ContractEngineRegistry::new());
    engines.register_defaults();
    engines.register(Arc::new(WasmEngine));
    let engine = engines.get("Wasm").expect("Wasm registered");
    let out = engine
        .execute(&ExecutionInput {
            tx: &tx,
            contract: &contract,
            sender_state: &user,
            token: &token,
            gas_limit: 0,
            storage: contract.storage.clone(),
            call_ctx: None,
        })
        .expect("wasm engine executes");
    let envelope: pneumatic_core::contracts::WasmResult =
        deserialize_rmp_to(&out.result_data).expect("wasm result envelope");
    assert_eq!(
        envelope.module_output,
        [5u8, b'h', b'e', b'l', b'l', b'o', 2, b'h', b'i', 0].to_vec(),
        "storage module output"
    );

    drive_pipeline_to_commit(&server, &rec, &tx, &account, &sender_pk).await;
    assert_committed_result_hash(&server, &token_id, &out.result_data, "wasm-storage");

    // The committer applies the storage delta to the contract token's
    // persisted state (the provider's copy — the W3 write-back).
    let stored = provider
        .get_token(&token_id, "token")
        .expect("provider serves the wasm token");
    let stored_contract = stored
        .get_asset::<SmartContract>()
        .expect("wasm token carries a contract asset");
    // Final op is the "a" tombstone → the key is gone.
    assert!(
        !stored_contract.storage.contains_key(&b"a"[..]),
        "the tombstoned key is removed from the persisted contract state"
    );
}

// --- (d) deployment (Spec + Wasm) -------------------------------------------

#[tokio::test]
async fn pipeline_deploy_spec_tx_creates_token_and_commits() {
    use pneumatic_core::contracts::deploy_contract;
    use pneumatic_core::contracts::{
        ContractEngine, ContractEngineRegistry, DeployParams,
    };

    let (account, sender_pk) = pipeline_sender();
    let program = pneumatic_core::contracts::InstructionProgram {
        version: 1,
        ops: vec![
            pneumatic_core::contracts::Op::LoadConst(42),
            pneumatic_core::contracts::Op::Emit,
        ],
    };
    let bytecode = serialize_to_bytes_rmp(&program).expect("spec program serializes");
    let params = DeployParams {
        name: "spec-deployed".to_string(),
        engine: "Spec".to_string(),
        bytecode: bytecode.clone(),
        metadata: Default::default(),
        owners: vec![],
        threshold: 0,
    };
    let payload = serialize_to_bytes_rmp(&params).expect("deploy params serialize");

    // The deploy tx targets the empty token id (the token being created).
    let placeholder = pipeline_token(vec![]);
    let provider = Arc::new(ContractE2eProvider::new(
        vec![(vec![], placeholder)],
        vec![(sender_pk.clone(), pipeline_user(&sender_pk))],
        vec![(own_staker_key(), 2_000)],
    ));

    let (_cfg, server, rec, _own_key, provider_ref) =
        contract_composite(provider, vec![]).await;

    let tx = pipeline_tx(
        &account,
        &sender_pk,
        "e2e_deploy_spec_1",
        "DeployContract",
        &vec![],
        &vec![],
        None,
        payload,
    );

    // Independent expected output: re-derive the CreateTokenDelta (the
    // committer does the same — a pure function of sender/nonce/params).
    let hash = BasicHashProvider::new();
    let delta = deploy_contract(&sender_pk, 1, &params, &hash).expect("deploy delta");
    let expected = serialize_to_bytes_rmp(&delta).expect("delta serializes");

    drive_pipeline_to_commit(&server, &rec, &tx, &account, &sender_pk).await;
    assert_committed_result_hash(&server, &vec![], &expected, "deploy-spec");

    // The committer applied the delta: the new token exists in the data
    // store with the deployed bytecode.
    let created = provider_ref
        .get_token(&delta.token_id, "test_env")
        .expect("deployed token exists in the data store");
    let created_contract = created
        .get_asset::<SmartContract>()
        .expect("deployed token carries a contract asset");
    assert_eq!(created_contract.bytecode, bytecode);
}

#[tokio::test]
async fn pipeline_deploy_wasm_tx_creates_token_and_commits() {
    use pneumatic_core::contracts::{deploy_contract, DeployParams};

    let (account, sender_pk) = pipeline_sender();
    let wasm_bytes = include_bytes!("../../../../src/contracts/wasm_fixtures/wasm_sum.wasm");
    let params = DeployParams {
        name: "wasm-deployed".to_string(),
        engine: "Wasm".to_string(),
        bytecode: wasm_bytes.to_vec(),
        metadata: Default::default(),
        owners: vec![],
        threshold: 0,
    };
    let payload = serialize_to_bytes_rmp(&params).expect("deploy params serialize");

    let placeholder = pipeline_token(vec![]);
    let provider = Arc::new(ContractE2eProvider::new(
        vec![(vec![], placeholder)],
        vec![(sender_pk.clone(), pipeline_user(&sender_pk))],
        vec![(own_staker_key(), 2_000)],
    ));

    let (_cfg, server, rec, _own_key, provider_ref) =
        contract_composite(provider, vec![]).await;

    let tx = pipeline_tx(
        &account,
        &sender_pk,
        "e2e_deploy_wasm_1",
        "DeployContract",
        &vec![],
        &vec![],
        None,
        payload,
    );

    let hash = BasicHashProvider::new();
    let delta = deploy_contract(&sender_pk, 1, &params, &hash).expect("deploy delta");
    let expected = serialize_to_bytes_rmp(&delta).expect("delta serializes");

    drive_pipeline_to_commit(&server, &rec, &tx, &account, &sender_pk).await;
    assert_committed_result_hash(&server, &vec![], &expected, "deploy-wasm");

    let created = provider_ref
        .get_token(&delta.token_id, "test_env")
        .expect("deployed token exists in the data store");
    let created_contract = created
        .get_asset::<SmartContract>()
        .expect("deployed token carries a contract asset");
    assert_eq!(created_contract.bytecode, wasm_bytes.to_vec());
}

// --- (e) cross-contract call (ADR-016 Model X) -------------------------------

/// A fixed [`TargetStateProvider`] for the independent expected-output
/// computation: resolves exactly the pinned target B (the same state the
/// executor's provider resolves through the data service).
#[derive(Clone)]
struct PinnedProvider {
    target: Token,
    contract: SmartContract,
    user: User,
}

impl TargetStateProvider for PinnedProvider {
    fn resolve(
        &self,
        target_token: &[u8],
        snapshot_ref: &pneumatic_core::contracts::SnapshotRef,
        _sender_key: &[u8],
    ) -> Result<PinnedTarget, pneumatic_core::contracts::ContractError> {
        if target_token != self.target.id {
            return Err(pneumatic_core::contracts::ContractError::InvalidInput(
                "pinned provider: wrong target".to_string(),
            ));
        }
        Ok(PinnedTarget {
            token: self.target.clone(),
            contract: self.contract.clone(),
            sender_state: self.user.clone(),
            snapshot_ref: snapshot_ref.clone(),
        })
    }
}

#[tokio::test]
async fn pipeline_cross_contract_call_tx_commits_with_call_status() {
    use pneumatic_core::contracts::{CallContext, ContractEngine, ContractEngineRegistry};

    let (account, sender_pk) = pipeline_sender();

    // Target B: a Spec contract that emits 42.
    let b_program = pneumatic_core::contracts::InstructionProgram {
        version: 1,
        ops: vec![
            pneumatic_core::contracts::Op::LoadConst(42),
            pneumatic_core::contracts::Op::Emit,
        ],
    };
    let b_bytecode = serialize_to_bytes_rmp(&b_program).expect("B program serializes");
    let target_b = contract_token(vec![0x0E], "Spec", "call-target", b_bytecode);
    let b_id = target_b.id.clone();
    let b_genesis_hash = target_b
        .blockchain
        .get_block_at(0)
        .expect("B genesis")
        .current_hash
        .clone();

    // Caller A: a Spec program that calls B at B's genesis pin and emits
    // the call status (1 = success).
    let a_program = pneumatic_core::contracts::InstructionProgram {
        version: 1,
        ops: vec![
            pneumatic_core::contracts::Op::Call {
                target_token: b_id.clone(),
                entry_point: String::new(),
                call_payload: vec![],
                ref_height: 0,
                ref_hash: b_genesis_hash.clone(),
            },
            pneumatic_core::contracts::Op::Emit,
        ],
    };
    let a_bytecode = serialize_to_bytes_rmp(&a_program).expect("A program serializes");
    let caller_a = contract_token(vec![0x0F], "Spec", "call-origin", a_bytecode);
    let a_id = caller_a.id.clone();

    let provider = Arc::new(ContractE2eProvider::new(
        vec![(a_id.clone(), caller_a), (b_id.clone(), target_b)],
        vec![(sender_pk.clone(), pipeline_user(&sender_pk))],
        vec![(own_staker_key(), 2_000)],
    ));

    let (_cfg, server, rec, _own_key, _provider) =
        contract_composite(provider, vec![]).await;

    let tx = pipeline_tx(
        &account,
        &sender_pk,
        "e2e_xcall_1",
        "ContractCall",
        &a_id,
        &vec![],
        Some(10),
        vec![],
    );

    // Independent expected output: run A's SpecEngine directly with a
    // pinned B — the call succeeds (B's program emits 42) and A emits the
    // status `1`.
    let token_cache = server.tokens();
    let a_token = token_cache.get(&a_id).expect("A in cache").clone();
    let a_contract = a_token
        .get_asset::<SmartContract>()
        .expect("A carries a contract asset");
    let b_token = token_cache.get(&b_id).expect("B in cache").clone();
    let b_contract = b_token
        .get_asset::<SmartContract>()
        .expect("B carries a contract asset");
    let user = pipeline_user(&sender_pk);
    let engines = Arc::new(ContractEngineRegistry::new());
    engines.register_defaults();
    let pinned = PinnedProvider {
        target: b_token.clone(),
        contract: b_contract.clone(),
        user: user.clone(),
    };
    let engine = engines.get("Spec").expect("Spec registered");
    let out = engine
        .execute(&ExecutionInput {
            tx: &tx,
            contract: &a_contract,
            sender_state: &user,
            token: &a_token,
            gas_limit: 0,
            storage: a_contract.storage.clone(),
            call_ctx: Some(Arc::new(CallContext::new(
                Arc::new(pinned),
                engines.clone(),
            ))),
        })
        .expect("caller spec engine executes");
    assert_eq!(
        out.result_data,
        1u64.to_le_bytes().to_vec(),
        "the call succeeds and A emits the status 1"
    );

    drive_pipeline_to_commit(&server, &rec, &tx, &account, &sender_pk).await;
    assert_committed_result_hash(&server, &a_id, &out.result_data, "xcall");
}

// --- (Phase 1) the client path: real socket, real ingress, real data service -

/// The Phase-1 exit test in miniature: ONE transaction, from ONE client, over
/// ONE real TCP socket into the ingress of a four-role composite, committed —
/// and then observed through the PRODUCTION data path.
///
/// Nothing here is a stub of the thing under test:
///
/// * the client is `pneumatic_client::TxClient` speaking HTTP over a real loopback
///   socket to the real `spawn_ingress_server` responder;
/// * the ingress sink is the shipped `NodeServer::ingress_sink`, dispatching the
///   node-signed `Verify` through the same `RoleDispatcher` the RNS bridge uses;
/// * the data layer is a real `pneumatic_data_service` behind the production
///   `DefaultDataProvider`, boot-seeded with real genesis (the same two boot
///   reads `data_service_boot.rs` pins), and the composite's token cache starts
///   EMPTY — `commit_block`'s lazy warm and its post-append `save_token` both
///   cross the socket;
/// * the terminal assertion reads the committed chain back through a *fresh*
///   provider against the service, and recomputes the expected result hash
///   outside the pipeline (the `selection_salt_through_the_production_data_provider`
///   discipline: prove the effect where production reads it, never through the
///   machinery that wrote it).
///
/// The inter-role relay (recorder → dispatch) is the established composite
/// fixture convention standing in for mesh gossip; everything else — wire,
/// HTTP, signatures, dispatch, execution, commit, persistence — is production.
#[tokio::test]
async fn client_transaction_through_the_ingress_commits_observable_through_the_data_service() {
    use pneumatic_client::{ClientError, TxClient};
    use pneumatic_core::conns::ConnTarget;
    use pneumatic_core::data::DefaultDataProvider;
    use pneumatic_core::ingress::spawn_ingress_server_on;
    use pneumatic_core::telemetry::{HealthState, Metrics};
    use pneumatic_data_service::{
        apply_genesis, spawn as boot_data_service, DataStore, GenesisNode, GenesisSpec,
    };

    // The composite's token partition, as `CONTRACT_SPEC` declares it — the
    // same partition the committer's gas deduction and the boot reads use.
    const PARTITION: &str = "token";

    // --- production data layer: real service, real client half ------------
    let store = Arc::new(DataStore::new());
    let (bound, accept_thread) = boot_data_service(
        "127.0.0.1:0".parse::<std::net::SocketAddr>().expect("loopback:0"),
        store.clone(),
        None,
    )
    .expect("the data service binds an ephemeral port");
    std::mem::forget(accept_thread); // keep the service alive for the test's scope

    let provider: Arc<dyn DataProvider> =
        Arc::new(DefaultDataProvider::new().with_source(ConnTarget::Remote(bound)));

    // Build the config FIRST — genesis must name the composite's own key,
    // and boot reads (stake snapshot, shielded pool) must not fail closed.
    let cfg = contract_runtime_config(vec![bad_peer()], type_config_floor(0), provider.clone());
    let own_key = cfg.public_key.clone();
    apply_genesis(
        &GenesisSpec {
            environment_id: "test_env".to_string(),
            token_partition_id: PARTITION.to_string(),
            stake_snapshot_epochs: vec![0, 1],
            shielded_root_recency: 10,
            nodes: vec![GenesisNode {
                public_key_hex: hex::encode(&own_key),
                stake: 2_000,
                fuel_balance: 100_000,
            }],
            accounts: Vec::new(),
            seed_shielded_pool: true,
            tokens: Vec::new(),
            seed_partition_token: true,
        },
        &*provider,
    )
    .expect("genesis applies against a live service");

    // --- the submitting account and the token, both in the SERVICE --------
    // (Same seed the `pneumatic-tx` CLI defaults to. The token exists ONLY in
    // the data service — the composite's cache starts empty, so every read
    // the pipeline performs on it, including `commit_block`'s warm, is a
    // socket read.)
    let account = Arc::new(Ed25519Provider::from_seed([0x42u8; 32]));
    let sender_pk = account.public_key().expect("client pubkey");
    provider
        .save_user(&sender_pk, pipeline_user(&sender_pk), PARTITION)
        .expect("sender user written through the production provider");
    let token = transfer_token(vec![0x0A]);
    let token_id = token.id.clone();
    provider
        .save_token(&token_id, token.clone(), PARTITION)
        .expect("genesis token written through the production provider");

    // --- the four-role composite over the production provider -------------
    // Wrapped in `Arc` exactly as the binary does — `ingress_sink` needs a
    // shared handle to dispatch to (the sink owns a clone of this Arc).
    let stake = Arc::new(MapStakeProvider::with_default(2_000));
    let server = Arc::new(build_runtime(cfg.clone(), stake, provider.clone()).expect("composite boots"));

    // Mesh gossip stands in: own key in all four buckets, each recording
    // what the previous role gossiped (the fixture convention of every
    // composite e2e in this file).
    let rec = fresh_recorders();
    let reg = server.node_registry();
    let c = |r: Arc<std::sync::Mutex<Vec<Vec<u8>>>>| {
        Box::new(RecordingConnection { recorder: r }) as Box<dyn Connection>
    };
    assert!(reg.register_peer(own_key.clone(), [1u8; 16], &NodeRegistryType::Sentinel, c(rec.sentinels.clone())), "sentinel peer");
    assert!(reg.register_peer(own_key.clone(), [2u8; 16], &NodeRegistryType::Executor, c(rec.executors.clone())), "executor peer");
    assert!(reg.register_peer(own_key.clone(), [3u8; 16], &NodeRegistryType::Finalizer, c(rec.finalizers.clone())), "finalizer peer");
    assert!(reg.register_peer(own_key.clone(), [4u8; 16], &NodeRegistryType::Committer, c(rec.committers.clone())), "committer peer");

    // --- the ingress on a real socket, wired to the shipped sink ----------
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("ingress bind");
    let (ingress_addr, _ingress) = spawn_ingress_server_on(
        listener,
        server.ingress_sink(),
        Arc::new(HealthState::new("e2e-ingress")),
        Arc::new(Metrics::new()),
    )
    .expect("ingress spawns");

    // --- the client: sign, POST, and be accepted --------------------------
    let client = TxClient::new(ingress_addr, "test_env", account.clone());
    let receipt = client
        .submit_transfer("e2e_ingress_1", &token_id, vec![0x77], 100, 1)
        .await
        .expect("the node accepts the client submission");
    assert_eq!(receipt.tx_id, "e2e_ingress_1");

    // --- relay the remaining pipeline hops (mesh stand-in) -----------------
    let exec_preload = next_recorded(&rec.executors, "Preload").await;
    server.dispatch(exec_preload).await.expect("executor Preload");
    let _ = next_recorded(&rec.finalizers, "PreloadForFinalizer").await;
    let fin_sign = next_recorded(&rec.finalizers, "Sign").await;
    server.dispatch(fin_sign).await.expect("finalizer Sign");
    let commit = next_recorded(&rec.committers, "Commit").await;
    server.dispatch(commit).await.expect("committer Commit");

    // --- the independent expectation, computed OUTSIDE the pipeline -------
    use pneumatic_core::contracts::{ContractEngine, ContractEngineRegistry};
    let contract = token.get_asset::<SmartContract>().expect("transfer token has a contract asset");
    let engines = Arc::new(ContractEngineRegistry::new());
    engines.register_defaults();
    let out = engines
        .get("Transfer")
        .expect("Transfer registered")
        .execute(&ExecutionInput {
            tx: &pipeline_tx(&*account, &sender_pk, "e2e_ingress_1", "Transfer", &token_id, &vec![0x77], Some(100), vec![]),
            contract: &contract,
            sender_state: &pipeline_user(&sender_pk),
            token: &token,
            gas_limit: 0,
            storage: contract.storage.clone(),
            call_ctx: None,
        })
        .expect("transfer engine executes");
    let expected_hash = BasicHashProvider::new().hash(&out.result_data);

    // --- TERMINAL: read the commit back through the production path -------
    // A FRESH provider against the same live service — nothing in-process
    // short of the socket can make this read succeed.
    let reader: Arc<dyn DataProvider> =
        Arc::new(DefaultDataProvider::new().with_source(ConnTarget::Remote(bound)));
    let committed = reader
        .get_token(&token_id, PARTITION)
        .expect("the committed chain is readable through the data service");
    assert_eq!(
        committed.blockchain.get_count(),
        2,
        "the service must hold genesis + the block THIS client's transaction committed"
    );
    let block = committed.blockchain.get_block_at(1).expect("committed block");
    assert_eq!(
        block.signed_trans.transaction.result_hash, expected_hash,
        "the committed block's result hash must match the engine output computed outside the pipeline"
    );
    assert_eq!(
        block.signed_trans.transaction.id, "e2e_ingress_1",
        "the block carries THIS client's transaction"
    );

    // The committer's live cache and the persisted chain are the same object
    // graph — they must not diverge at rest.
    assert_eq!(
        server.tokens().get(&token_id).expect("cache warmed by commit").blockchain.get_count(),
        2,
        "the committer cache and the persisted chain agree"
    );

    // --- the honest negative: a foreign chain never reaches the pipeline --
    let foreign = TxClient::new(ingress_addr, "other_env", account.clone());
    let err = foreign
        .submit_transfer("e2e_ingress_wrong_chain", &token_id, vec![0x77], 100, 1)
        .await
        .expect_err("a submission naming a foreign chain must be refused");
    match err {
        ClientError::Rejected { status, .. } => assert_eq!(status, 422, "chain mismatch is a pipeline-level refusal: {err}"),
        other => panic!("expected a 422 refusal, got {other:?}"),
    }
}

// --- the self-delivery pin: the same pipeline with the relay REMOVED ---------

/// `fact-composite-no-self-delivery`, pinned. The test above is the composite
/// suite's established shape: every role-to-role hop is *replayed by the test*
/// from what a recording connection caught — a mesh stand-in that was itself
/// the missing production capability. This test is the same production path
/// (real client, real socket, real ingress, real data service, four live role
/// plugins) with the stand-in **removed**: nothing registers this host's own key
/// in a bucket, and the test dispatches exactly one message — the client's
/// submission, through the ingress.
///
/// So every remaining hop (`Preload` → `PreloadForFinalizer` → `Sign` →
/// `Commit`) has to reach the role this same host runs by way of the bridge's
/// self-subscription, or the transaction sits in the pool forever and the chain
/// never grows. Both halves are load-bearing: the fan-out has to deliver the
/// local copy, and the receiving handler's sender gate has to believe that this
/// host's own key holds the role it fans out as.
///
/// Adding a relay here would make this test prove nothing. It is the unit-test
/// form of the one proof that matters for Phase 2: the four-container rehearsal
/// growing a chain with no test-side machinery in the loop.
#[tokio::test]
async fn a_composite_pipeline_commits_with_no_test_side_relay() {
    use pneumatic_client::TxClient;
    use pneumatic_core::conns::ConnTarget;
    use pneumatic_core::data::DefaultDataProvider;
    use pneumatic_core::ingress::spawn_ingress_server_on;
    use pneumatic_core::telemetry::{HealthState, Metrics};
    use pneumatic_data_service::{
        apply_genesis, spawn as boot_data_service, DataStore, GenesisNode, GenesisSpec,
    };

    const PARTITION: &str = "token";

    // --- production data layer ---------------------------------------------
    let store = Arc::new(DataStore::new());
    let (bound, accept_thread) = boot_data_service(
        "127.0.0.1:0".parse::<std::net::SocketAddr>().expect("loopback:0"),
        store.clone(),
        None,
    )
    .expect("the data service binds an ephemeral port");
    std::mem::forget(accept_thread);

    let provider: Arc<dyn DataProvider> =
        Arc::new(DefaultDataProvider::new().with_source(ConnTarget::Remote(bound)));

    let cfg = contract_runtime_config(vec![bad_peer()], type_config_floor(0), provider.clone());
    let own_key = cfg.public_key.clone();
    apply_genesis(
        &GenesisSpec {
            environment_id: "test_env".to_string(),
            token_partition_id: PARTITION.to_string(),
            stake_snapshot_epochs: vec![0, 1],
            shielded_root_recency: 10,
            nodes: vec![GenesisNode {
                public_key_hex: hex::encode(&own_key),
                stake: 2_000,
                fuel_balance: 100_000,
            }],
            accounts: Vec::new(),
            seed_shielded_pool: true,
            tokens: Vec::new(),
            seed_partition_token: true,
        },
        &*provider,
    )
    .expect("genesis applies against a live service");

    let account = Arc::new(Ed25519Provider::from_seed([0x42u8; 32]));
    let sender_pk = account.public_key().expect("client pubkey");
    provider
        .save_user(&sender_pk, pipeline_user(&sender_pk), PARTITION)
        .expect("sender user written through the production provider");
    let token = transfer_token(vec![0x0A]);
    let token_id = token.id.clone();
    provider
        .save_token(&token_id, token.clone(), PARTITION)
        .expect("genesis token written through the production provider");

    // --- the four-role composite -------------------------------------------
    let stake = Arc::new(MapStakeProvider::with_default(2_000));
    let server =
        Arc::new(build_runtime(cfg.clone(), stake, provider.clone()).expect("composite boots"));

    // Four roles installed means the self-subscription covers all four — the
    // bridge does both from the same list. Asserted rather than assumed, because
    // everything below is only a statement about self-delivery if this is true.
    assert_eq!(
        server.node_registry().self_delivery_roles().len(),
        4,
        "the bridge subscribed the host to its own fan-out for every role it installed"
    );

    // --- and NO bucket entries for this host's own key. That is the whole test:
    // the peer path has nowhere to carry a same-host hop, so only self-delivery
    // can complete the pipeline.
    for role in [
        NodeRegistryType::Committer,
        NodeRegistryType::Sentinel,
        NodeRegistryType::Executor,
        NodeRegistryType::Finalizer,
    ] {
        assert!(
            server
                .node_registry()
                .get_nodes(&role)
                .map(|nodes| nodes.is_empty())
                .unwrap_or(true),
            "the {role:?} bucket must hold no peers — a registered self would be \
             a relay wearing a fixture's clothes"
        );
    }

    // --- exactly one message enters this process: the client's submission ----
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("ingress bind");
    let (ingress_addr, _ingress) = spawn_ingress_server_on(
        listener,
        server.ingress_sink(),
        Arc::new(HealthState::new("e2e-no-relay")),
        Arc::new(Metrics::new()),
    )
    .expect("ingress spawns");

    let client = TxClient::new(ingress_addr, "test_env", account.clone());
    let receipt = client
        .submit_transfer("no_relay_1", &token_id, vec![0x77], 100, 1)
        .await
        .expect("the node accepts the client submission");
    assert_eq!(receipt.tx_id, "no_relay_1");

    // --- the independent expectation, computed OUTSIDE the pipeline ---------
    use pneumatic_core::contracts::{ContractEngine, ContractEngineRegistry};
    let contract = token
        .get_asset::<SmartContract>()
        .expect("transfer token has a contract asset");
    let engines = Arc::new(ContractEngineRegistry::new());
    engines.register_defaults();
    let out = engines
        .get("Transfer")
        .expect("Transfer registered")
        .execute(&ExecutionInput {
            tx: &pipeline_tx(
                &*account,
                &sender_pk,
                "no_relay_1",
                "Transfer",
                &token_id,
                &vec![0x77],
                Some(100),
                vec![],
            ),
            contract: &contract,
            sender_state: &pipeline_user(&sender_pk),
            token: &token,
            gas_limit: 0,
            storage: contract.storage.clone(),
            call_ctx: None,
        })
        .expect("transfer engine executes");
    let expected_hash = BasicHashProvider::new().hash(&out.result_data);

    // --- TERMINAL: the committed chain, read back through a FRESH provider --
    // The hops are asynchronous with no relay to synchronise on, so this polls
    // the production read path until the block shows up: the same terminal
    // assertion as the relayed test, without the test driving the finish.
    let reader: Arc<dyn DataProvider> =
        Arc::new(DefaultDataProvider::new().with_source(ConnTarget::Remote(bound)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let committed = loop {
        let read = reader
            .get_token(&token_id, PARTITION)
            .expect("the token is readable through the data service");
        if read.blockchain.get_count() >= 2 {
            break read;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no block committed — the pipeline stalled with no relay in the loop. \
             committer cache chain length = {}",
            server
                .tokens()
                .get(&token_id)
                .map(|t| t.blockchain.get_count())
                .unwrap_or(0),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    assert_eq!(
        committed.blockchain.get_count(),
        2,
        "genesis + the block THIS transaction committed, with no relay in the loop"
    );
    let block = committed.blockchain.get_block_at(1).expect("committed block");
    assert_eq!(
        block.signed_trans.transaction.result_hash, expected_hash,
        "the committed block's result hash matches the engine output computed outside the pipeline"
    );
    assert_eq!(
        block.signed_trans.transaction.id, "no_relay_1",
        "the block carries THIS client's transaction"
    );
}
