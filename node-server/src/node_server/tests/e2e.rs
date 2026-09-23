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
