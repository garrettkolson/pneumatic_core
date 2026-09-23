//! Shielded-transfer tests: quorum commit with a shielded block, vote
//! hash mismatch, spec-less finalizer, unregistered-voter rejection, and the
//! live full-path fixture.
use super::helpers::*;
use super::super::*;

// -----------------------------------------------------------------------
// Phase S5.2 discriminators — finalizer signs the public outputs
// -----------------------------------------------------------------------

/// Full-path S5.2 fixture: the finalizer's **own** identity is the voting
/// finalizer, registered in the node registry as a `Finalizer` peer (the
/// C1 gate) on a **recording** connection (so the fanned-out `ShieldedVote`
/// is capturable), with a 100-stake snapshot for that key (100/100 ≥ 67%
/// — one vote is quorum), a pool view that accepts the fixture tx's
/// referenced root, the stub-spec env, and a committer peer whose
/// recording connection captures the `Commit` / `BlockFinalized`
/// dispatches.
fn make_shielded_full_path_fixture(
    stx_root: [u8; 32],
) -> (
    Finalizer,
    Arc<NodeIdentity>,
    Arc<std::sync::Mutex<Vec<Vec<u8>>>>, // finalizer peer (own key) — vote capture
    Arc<std::sync::Mutex<Vec<Vec<u8>>>>, // committer peer — commit capture
) {
    let identity = Arc::new(NodeIdentity::generate_in_memory());
    let registry = make_test_node_registry();

    // The voting finalizer (the finalizer's own key) as a Finalizer peer
    // on a recording connection.
    registry
        .get_config()
        .type_configs
        .insert(
            NodeRegistryType::Finalizer,
            pneumatic_core::node::NodeTypeConfig { min: 0, max: 10, min_stake: 0 },
        );
    let voter_pk = identity.ed25519.public_key().expect("voter public key");
    let finalizer_recorder = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        registry.register_peer(
            voter_pk.clone(),
            identity.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { recorder: finalizer_recorder.clone() }),
        ),
        "voting finalizer registers within capacity"
    );

    // A committer peer to capture the commit dispatch.
    registry
        .get_config()
        .type_configs
        .insert(
            NodeRegistryType::Committer,
            pneumatic_core::node::NodeTypeConfig { min: 0, max: 10, min_stake: 0 },
        );
    let committer_recorder = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        registry.register_peer(
            vec![0xCB; 32],
            [0xCBu8; 16],
            &NodeRegistryType::Committer,
            Box::new(RecordingConnection { recorder: committer_recorder.clone() }),
        ),
        "committer registers within capacity"
    );

    // 100/100 stake for the voter — a single vote reaches the 67%
    // stake-weighted quorum.
    let data_provider = Arc::new(
        StubDataProvider::new().with_stake_snapshot(0, make_stake_set(vec![(voter_pk, 100)])),
    );
    let finalizer = make_finalizer_with_shielded_identity(
        registry,
        make_test_pending_registry(),
        data_provider,
        pool_view_with_root(stx_root),
        env_with_stub_shielded_spec(),
        identity.clone(),
    );
    (finalizer, identity, finalizer_recorder, committer_recorder)
}

/// S5.2 discriminator 1 — quorum → commit: the full shielded pipeline
/// (sign the public outputs → fan the vote → collect at stake quorum →
/// build the shielded block → commit) produces a `TransactionCommit`
/// whose block carries `shielded: Some(stx)` and the vote in
/// `executor_sigs` keyed by the voting finalizer's public key.
#[tokio::test]
async fn shielded_quorum_produces_commit_with_shielded_block() {
    let stx = make_shielded_tx_fixture("stx-quorum", [9u8; 32]);
    let (finalizer, identity, finalizer_recorder, committer_recorder) =
        make_shielded_full_path_fixture(stx.merkle_root);

    // (a) SignShielded → the finalizer signs stx.hash() and fans the vote
    //     out to finalizer peers (captured on its own recording conn).
    let body = serialize_to_bytes_rmp(&stx).expect("stx serializes");
    let sign_msg = Message::signed("test_env".to_string(), "SignShielded", body, None, &identity)
        .expect("sign message");
    finalizer
        .handle_sign_shielded(&sign_msg)
        .await
        .expect("SignShielded handled");

    let vote_msg = captured_message(&finalizer_recorder, "ShieldedVote");
    assert_eq!(vote_msg.action, "ShieldedVote", "the fanned-out action is the vote");

    // (b) The collector finalizes at stake quorum and dispatches the commit.
    finalizer
        .handle_shielded_vote(&vote_msg)
        .await
        .expect("vote reaches quorum → finalizes");

    // (c) The committer received a Commit carrying the shielded tx and the
    //     vote, and the BlockFinalized tail went out too.
    let commit_msg = captured_message(&committer_recorder, "Commit");
    let commit: TransactionCommit =
        deserialize_rmp_to(&commit_msg.body).expect("commit body deserializes");
    assert_eq!(
        commit.trans_id,
        stx.id.as_bytes().to_vec(),
        "commit is for the shielded tx"
    );
    assert_eq!(
        commit.proposed_block.signed_trans.shielded,
        Some(stx.clone()),
        "the committed block carries the shielded tx"
    );
    let voter_pk = identity.ed25519.public_key().expect("voter pk");
    let vote = commit
        .proposed_block
        .signed_trans
        .executor_sigs
        .get(&voter_pk)
        .expect("the vote rides in executor_sigs keyed by the voting finalizer");
    let captured_vote: TransactionSignature =
        deserialize_rmp_to(&vote_msg.body).expect("vote body deserializes");
    assert_eq!(vote.signature, captured_vote.signature, "vote signature round-trips");
    assert_eq!(vote.current_stake, 100, "stake is stamped from the snapshot");
    assert!(
        !captured_message_absent(&committer_recorder, "BlockFinalized"),
        "the BlockFinalized tail is dispatched as well"
    );

    // (d) Cleanup ran: the recorded bytes are gone after finalization.
    assert!(
        finalizer.shielded_transactions.lock().await.is_empty(),
        "finalization cleans up the recorded-bytes store"
    );
}

/// S5.2 discriminator 2 — mutate a commitment after signatures: a vote
/// binding `stx.hash()` of the *original* bytes, delivered after the
/// recorded bytes were tampered with, aborts finalization fail-closed —
/// no commit is dispatched and the recorded bytes are left intact.
#[tokio::test]
async fn shielded_vote_hash_mismatch_aborts_finalization() {
    let stx = make_shielded_tx_fixture("stx-mutated", [9u8; 32]);
    let (finalizer, identity, _finalizer_recorder, committer_recorder) =
        make_shielded_full_path_fixture(stx.merkle_root);

    // Record the original canonical bytes via the real SignShielded path.
    let body = serialize_to_bytes_rmp(&stx).expect("stx serializes");
    let sign_msg = Message::signed("test_env".to_string(), "SignShielded", body, None, &identity)
        .expect("sign message");
    finalizer
        .handle_sign_shielded(&sign_msg)
        .await
        .expect("SignShielded handled");

    // Tamper: same id, a different commitment → different canonical bytes.
    let mut mutated = stx.clone();
    mutated.commitments = vec![[0xEEu8; 32]];
    let mutated_bytes = mutated.canonical_bytes().expect("mutated stx canonical bytes");
    finalizer
        .shielded_transactions
        .lock()
        .await
        .insert(stx.id.clone(), mutated_bytes.clone());

    // A vote that genuinely binds the original hash (inner + envelope
    // signatures both valid) — only the recorded-bytes integrity check
    // can catch it.
    let original_hash = stx.hash().expect("original stx hash");
    let inner_sig = identity
        .ed25519
        .sign_data(&original_hash)
        .expect("voter signs the hash");
    let vote = TransactionSignature {
        transaction_id: stx.id.as_bytes().to_vec(),
        env_id: b"test_env".to_vec(),
        transaction_hash: original_hash,
        signature: inner_sig,
        current_stake: 0, // stamped from the snapshot by the handler
    };
    let vote_body = serialize_to_bytes_rmp(&vote).expect("vote serializes");
    let vote_msg =
        Message::signed("test_env".to_string(), "ShieldedVote", vote_body, None, &identity).expect("vote msg");

    let result = finalizer.handle_shielded_vote(&vote_msg).await;
    assert!(
        matches!(&result, Err(PneumaticError::CryptoError(s)) if s.contains("binds a hash different")),
        "hash mismatch must abort finalization fail-closed, got {result:?}"
    );
    // No commit left the node, and the recorded bytes are untouched.
    assert!(
        captured_message_absent(&committer_recorder, "Commit"),
        "no Commit may be dispatched on a hash mismatch"
    );
    assert_eq!(
        finalizer
            .shielded_transactions
            .lock()
            .await
            .get(&stx.id)
            .map(|b| b.len()),
        Some(mutated_bytes.len()),
        "recorded bytes stay intact on abort (no cleanup on failure)"
    );
}

/// S5.2 discriminator 3 — a finalizer that can't verify sends no vote:
/// with no `Shielded` spec in its environment, `handle_sign_shielded`
/// fails at the advisory re-verification — no vote is fanned out, no
/// canonical bytes are recorded, and quorum is unreachable.
#[tokio::test]
async fn shielded_finalizer_without_spec_sends_no_vote() {
    let stx = make_shielded_tx_fixture("stx-nospec", [9u8; 32]);
    let identity = Arc::new(NodeIdentity::generate_in_memory());
    let registry = make_test_node_registry();
    registry
        .get_config()
        .type_configs
        .insert(
            NodeRegistryType::Finalizer,
            pneumatic_core::node::NodeTypeConfig { min: 0, max: 10, min_stake: 0 },
        );
    let voter_pk = identity.ed25519.public_key().expect("voter pk");
    let finalizer_recorder = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        registry.register_peer(
            voter_pk.clone(),
            identity.rhash,
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { recorder: finalizer_recorder.clone() }),
        ),
        "voting finalizer registers"
    );
    // Plain env — the `Shielded` spec is NOT registered (the JSON env
    // spec path cannot register it; this is the fail-closed default).
    let finalizer = make_finalizer_with_shielded_identity(
        registry,
        make_test_pending_registry(),
        Arc::new(StubDataProvider::new().with_stake_snapshot(0, make_stake_set(vec![(voter_pk, 100)]))),
        pool_view_with_root(stx.merkle_root),
        test_env_data(),
        identity.clone(),
    );

    let body = serialize_to_bytes_rmp(&stx).expect("stx serializes");
    let sign_msg = Message::signed("test_env".to_string(), "SignShielded", body, None, &identity).expect("msg");
    let result = finalizer.handle_sign_shielded(&sign_msg).await;
    assert!(
        matches!(&result, Err(PneumaticError::Validation(reasons)) if reasons == &vec![ValidationFailureReason::UnsupportedAction]),
        "missing spec must fail the advisory validation fail-closed, got {result:?}"
    );
    assert!(
        captured_message_absent(&finalizer_recorder, "ShieldedVote"),
        "no vote is fanned out when this finalizer cannot verify"
    );
    assert!(
        finalizer.shielded_transactions.lock().await.is_empty(),
        "no canonical bytes are recorded on a failed advisory check"
    );
}

/// S5.2 discriminator 4 — unknown or role-less voter: a vote whose
/// envelope is signed by a key that is (a) registered under no role, or
/// (b) registered as an `Executor` (the standard path's role), is
/// rejected at the C1 gate — the voter's key never enters the signature
/// registry and no commit can result.
#[tokio::test]
async fn shielded_vote_from_unregistered_or_non_finalizer_voter_rejected() {
    // A full-path fixture so the tx is recorded and a real quorum is
    // reachable if (and only if) a *valid* vote arrives.
    let stx = make_shielded_tx_fixture("stx-unregistered", [9u8; 32]);
    let (finalizer, identity, _finalizer_recorder, committer_recorder) =
        make_shielded_full_path_fixture(stx.merkle_root);
    let body = serialize_to_bytes_rmp(&stx).expect("stx serializes");
    let sign_msg = Message::signed("test_env".to_string(), "SignShielded", body, None, &identity).expect("msg");
    finalizer.handle_sign_shielded(&sign_msg).await.expect("recorded");

    let stx_hash = stx.hash().expect("stx hash");

    // (a) Unregistered signer → "not registered as any node".
    let stranger = Arc::new(NodeIdentity::generate_in_memory());
    let stranger_pk = stranger.ed25519.public_key().expect("stranger pk");
    let vote = TransactionSignature {
        transaction_id: stx.id.as_bytes().to_vec(),
        env_id: b"test_env".to_vec(),
        transaction_hash: stx_hash.clone(),
        signature: stranger.ed25519.sign_data(&stx_hash).expect("sign"),
        current_stake: 0,
    };
    let vote_msg = Message::signed(
        "test_env".to_string(),
        "ShieldedVote",
        serialize_to_bytes_rmp(&vote).expect("vote serializes"),
        None,
        &stranger,
    )
    .expect("vote msg");
    let result = finalizer.handle_shielded_vote(&vote_msg).await;
    assert!(
        matches!(&result, Err(PneumaticError::Registry(s)) if s.contains("not registered as any node")),
        "unregistered voter must be rejected, got {result:?}"
    );

    // (b) Registered, but as an Executor (the non-shielded role) →
    //     "not a Finalizer" — the shielded arms gate on the Finalizer
    //     role specifically (roadmap 2.1: the Executor stage is skipped).
    let executor_voter = Arc::new(NodeIdentity::generate_in_memory());
    let executor_pk = executor_voter.ed25519.public_key().expect("executor pk");
    finalizer
        .node_registry
        .get_config()
        .type_configs
        .insert(
            NodeRegistryType::Executor,
            pneumatic_core::node::NodeTypeConfig { min: 0, max: 10, min_stake: 0 },
        );
    assert!(
        finalizer
            .node_registry
            .register_peer(
                executor_pk.clone(),
                executor_voter.rhash,
                &NodeRegistryType::Executor,
                Box::new(NoOpConnection),
            ),
        "executor registers"
    );
    let vote = TransactionSignature {
        transaction_id: stx.id.as_bytes().to_vec(),
        env_id: b"test_env".to_vec(),
        transaction_hash: stx_hash.clone(),
        signature: executor_voter.ed25519.sign_data(&stx_hash).expect("sign"),
        current_stake: 0,
    };
    let vote_msg = Message::signed(
        "test_env".to_string(),
        "ShieldedVote",
        serialize_to_bytes_rmp(&vote).expect("vote serializes"),
        None,
        &executor_voter,
    )
    .expect("vote msg");
    let result = finalizer.handle_shielded_vote(&vote_msg).await;
    assert!(
        matches!(&result, Err(PneumaticError::Registry(s)) if s.contains("not a Finalizer")),
        "an Executor-registered voter must be rejected on the shielded arms, got {result:?}"
    );

    // Neither rejected vote entered the collector; no commit dispatched.
    assert_eq!(
        finalizer.signature_collector.signature_count(&stx.id),
        0,
        "rejected votes leave the collector untouched"
    );
    assert!(captured_message_absent(&committer_recorder, "Commit"));
}

/// S5.2 discriminator 5 — the constructed shielded block validates under
/// `SelfSignedBlockValidatorSpec` (the in-tree fact the plan locks: the
/// non-empty `executor_sigs` requirement lives only in
/// `ExecutedBlockValidatorSpec`, so a shielded block with its S3.1
/// placeholder `Transaction` passes the SelfSigned block spec).
#[tokio::test]
async fn shielded_block_passes_self_signed_block_spec() {
    let stx = make_shielded_tx_fixture("stx-selfsigned", [9u8; 32]);
    let (finalizer, identity, finalizer_recorder, committer_recorder) =
        make_shielded_full_path_fixture(stx.merkle_root);

    // Run the full pipeline to obtain the committed block.
    let body = serialize_to_bytes_rmp(&stx).expect("stx serializes");
    let sign_msg = Message::signed("test_env".to_string(), "SignShielded", body, None, &identity).expect("msg");
    finalizer.handle_sign_shielded(&sign_msg).await.expect("sign");
    let vote_msg = captured_message(&finalizer_recorder, "ShieldedVote");
    finalizer.handle_shielded_vote(&vote_msg).await.expect("finalize");

    let commit_msg = captured_message(&committer_recorder, "Commit");
    let commit: TransactionCommit =
        deserialize_rmp_to(&commit_msg.body).expect("commit body");
    let block = commit.proposed_block;
    assert!(block.signed_trans.shielded.is_some(), "the block is shielded");

    // A self-verified token with an empty chain: the fixture's data
    // provider has no token, so `resolve_previous_hash` fell back to the
    // genesis empty prev-hash, and the genesis convention (empty
    // `last_hash_in` ⇔ empty `previous_hash`) links it to an empty chain.
    let token = Token::new()
        .with_id(stx.token_id.clone())
        .with_is_self_verified(true);

    let spec = pneumatic_core::validation::SelfSignedBlockValidatorSpec::new();
    let env = finalizer.env_data.clone();
    pneumatic_core::validation::BlockValidatorSpec::validate(&spec, &block, &token, &env)
        .expect("shielded block must validate under the SelfSigned block spec");
}
