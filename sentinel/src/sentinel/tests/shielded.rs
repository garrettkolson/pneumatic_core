//! Shielded tests for the Sentinel, moved from `crate::sentinel::tests`.

use super::helpers::*;
use super::super::*;

// -----------------------------------------------------------------------
// Phase S5.1 — the shielded transfer handler (the real path; the S3.2
// stub is retired).
//
// The stub spec (plan, Decision 6) pins the handler's wiring without
// paying halo2 keygen: its registration name is the real
// `ShieldedValidationSpec::NAME` constant (the seam the handler looks up,
// Decision 3) and its `validate_shielded` is cheap — empty proof ⇒
// `InvalidShieldedProof`, else `valid` with zero risk. The real
// `ShieldedValidationSpec`'s cryptographic behavior is pinned in the core
// suite (S4.1.5); it appears here only in the two stale-state tests and
// the `#[ignore]`d live end-to-end, each built to fail at checks 2/3
// before check 4's lazy verifier (no keygen in the default run).
// -----------------------------------------------------------------------

use pneumatic_core::conns::Connection;
use pneumatic_core::crypto::Ed25519Provider;
use pneumatic_core::epoch::StakeSet;
use group::GroupEncoding;
use pasta_curves::pallas::{Base as Fp, Scalar as Fq};
use ff::PrimeField; // `to_repr()` in the stale-root test

/// A `Connection` that records every "sent" payload byte-verbatim (same
/// shape as the transaction_notifier test helpers).
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


/// `send_to_nodes` dispatches on a detached thread, so recorder counts are
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


/// The outbound message is signed with the sentinel's own identity (the
/// receiver verifies against the sender's registered key).
fn assert_signed_by(message: &Message, public_key: &[u8]) {
    assert_eq!(
        message.public_key,
        public_key.to_vec(),
        "message.public_key must be the sender's (sentinel's) identity key"
    );
    let verifier = Ed25519Provider::generate();
    let ok = verifier
        .check_signature(&message.signature, &message.public_key, &message.body)
        .expect("signature check should succeed");
    assert!(ok, "message body must verify under the sender's identity key");
}

/// Test-only shielded spec (Decision 6). See the section comment above for
/// the role it plays.
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


/// Rebuild the env's transaction-spec registry with a fresh one built by
/// `build`, applied **before** the `Arc` (registry `register` /
/// `register_shielded` take `&mut self`) — the same shape the boot path
/// uses (`environment.rs:220-265`).
fn env_with_specs(
    env: EnvironmentMetadata,
    build: impl FnOnce(&mut ValidationSpecRegistry),
) -> EnvironmentMetadata {
    let mut env = env;
    let mut registry = ValidationSpecRegistry::new();
    registry.register_defaults();
    build(&mut registry);
    env.transaction_validation_specs = Arc::new(registry);
    env
}


/// Stub-spec env: the defaults plus the stub registered under the real
/// name constant (the lookup seam).
fn env_with_stub_shielded_spec() -> EnvironmentMetadata {
    env_with_specs(make_test_env_data(), |reg| reg.register(Box::new(StubShieldedSpec)))
}


/// Real-spec env: the defaults plus `register_shielded`.
fn env_with_real_shielded_spec() -> EnvironmentMetadata {
    env_with_specs(make_test_env_data(), |reg| reg.register_shielded())
}


/// A self-verified, shielded-opt-in token (both gates of step 3 pass).
fn make_opt_in_token() -> Token {
    let mut token = Token::new();
    token.is_self_verified = true;
    token.set_metadata("shielded_opt_in".to_string(), "true".to_string());
    token
}


/// The S4.1 canonical fixture (validation.rs:1389): commitments and root
/// are real field encodings, so `public_inputs_from_shielded_tx` decodes
/// cleanly; the proof is a placeholder buffer. Checks 1→3 short-circuit
/// before check 4, so the placeholder never reaches the real verifier in
/// the stale-state tests.
fn make_shielded_tx_fixture(id: &str) -> ShieldedTransaction {
    let note = ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) };
    let spend_key = [0xABu8; 32];
    let output_note = ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3), rcm: Fq::from(4) };
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root, _proof) = tree.append(&commit(&note));
    let spent_commit: [u8; 32] = commit(&note).to_bytes().as_ref().try_into().unwrap();
    let output_commit: [u8; 32] = commit(&output_note).to_bytes().as_ref().try_into().unwrap();
    let nullifier_bytes = nullifier(&note, &spend_key);
    ShieldedTransaction {
        id: id.to_string(),
        action: "ShieldedTransfer".into(),
        token_id: vec![1],
        spent_commitments: vec![spent_commit],
        nullifiers: vec![nullifier_bytes],
        commitments: vec![output_commit],
        merkle_root: root_to_bytes(&root),
        proof: vec![0u8; 64],
        note_ciphertexts: vec![vec![0u8; 64]],
        fee: 0,
    }
}


/// The S3.2 wire fixture, verbatim: a wire-serializable
/// `ShieldedTransaction` for the stub-spec tests (the stub only inspects
/// `proof`).
fn make_stub_shielded_tx(id: &str) -> ShieldedTransaction {
    ShieldedTransaction {
        id: id.to_string(),
        action: "ShieldedTransfer".to_string(),
        token_id: vec![9, 10],
        spent_commitments: vec![[1u8; 32]],
        nullifiers: vec![[1u8; 32]],
        commitments: vec![[2u8; 32]],
        merkle_root: [3u8; 32],
        proof: vec![4, 5, 6],
        note_ciphertexts: vec![vec![7u8; 128]],
        fee: 0,
    }
}


/// Wrap a `ShieldedTransaction` body in the inner `"ShieldedTransfer"`
/// `Message` the composite plugin delivers.
fn shielded_transfer_message(body: Vec<u8>) -> Vec<u8> {
    let message = Message {
        chain_id: "env".to_string(),
        action: "ShieldedTransfer".to_string(),
        body,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };
    serialize_to_bytes_rmp(&message).expect("inner message serializes")
}


/// Assert a `SentinelError::Validation` carrying the exact fail-closed
/// reason.
fn expect_validation_reason(err: SentinelError, reason: ValidationFailureReason) {
    match err {
        SentinelError::Validation(PneumaticError::Validation(reasons)) => assert!(
            reasons.iter().any(|r| *r == reason),
            "expected {:?} among the validation reasons, got {reasons:?}",
            reason
        ),
        other => panic!("expected Validation({:?}), got {other:?}", reason),
    }
}


/// Full shielded-path fixture (Decision 6): stub data provider + env
/// (built before the `Arc`) + shared pool view, a registered finalizer
/// peer (recording) and an executor peer (recording — proof of absence of
/// executor dispatch), and a non-empty epoch-1 stake snapshot so
/// `assign_finalizer_deterministic` has something to select from.
/// Returns the sentinel, the tx registry, the finalizer and executor
/// recorders, and the sentinel's public key (for signature assertions).
fn make_shielded_fixture(
    data_provider: StubDataProvider,
    env: EnvironmentMetadata,
    view: Arc<dyn ShieldedPoolView>,
) -> (
    Sentinel,
    Arc<PendingTransactionRegistry>,
    Arc<Mutex<Vec<Vec<u8>>>>,
    Arc<Mutex<Vec<Vec<u8>>>>,
    Vec<u8>,
) {
    let config = make_test_config();
    // `NodeIdentity` is not `Clone` (it is Arc-ed into the config), so the
    // fixture hands out the config's public key — the same key
    // `assert_signed_by` would derive from the identity.
    let public_key = config.public_key.clone();
    let node_registry = make_test_node_registry();
    let registry = Arc::new(PendingTransactionRegistry::new());
    let env_data = Arc::new(env);
    let gossiper = Arc::new(Gossiper::new(
        NodeRegistryType::Sentinel,
        make_test_config(),
        300,
        env_data.asym_crypto_provider.clone(),
    ));
    let sentinel = Sentinel::new(
        config,
        env_data,
        node_registry,
        registry.clone(),
        gossiper,
        Arc::new(data_provider),
        view,
    );

    let finalizer_recorder = Arc::new(Mutex::new(Vec::new()));
    let executor_recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(
        sentinel.node_registry.register_peer(
            vec![0xF1; 32],
            [1u8; 16],
            &NodeRegistryType::Finalizer,
            Box::new(RecordingConnection { recorder: finalizer_recorder.clone() }),
        ),
        "finalizer peer registers"
    );
    assert!(
        sentinel.node_registry.register_peer(
            vec![0xE2; 32],
            [2u8; 16],
            &NodeRegistryType::Executor,
            Box::new(RecordingConnection { recorder: executor_recorder.clone() }),
        ),
        "executor peer registers"
    );
    sentinel.stake_snapshot_cache.put(
        1,
        StakeSet {
            stakers: [(vec![0xF1; 32], 100)].into_iter().collect(),
        },
    );

    (sentinel, registry, finalizer_recorder, executor_recorder, public_key)
}


/// S5.1 step 1: a malformed body is an `Encoding` error before any
/// spec/token/registry work — nothing is registered, nothing is sent.
#[test]
fn shielded_transfer_malformed_body_is_encoding_error() {
    let (sentinel, registry) = make_sentinel_fixture();

    let raw = shielded_transfer_message(b"this is not a shielded transaction".to_vec());
    match sentinel.on_data_received(raw) {
        Err(SentinelError::Encoding(_)) => {}
        other => panic!("expected an Encoding error, got {other:?}"),
    }
    assert!(!registry.contains_shielded("anything"), "a malformed body registers nothing");
}


/// S5.1 step 2, spec lookup: a deployment that never
/// `register_shielded` gets `Validation([UnsupportedAction])` — no
/// fallback to any other spec (Decision 3). Nothing is registered.
#[test]
fn shielded_transfer_missing_spec_is_unsupported_action() {
    // The bare fixture's env carries only the default specs (no "Shielded").
    let (sentinel, registry) = make_sentinel_fixture();

    let body = serialize_to_bytes_rmp(&make_stub_shielded_tx("tx-no-spec")).expect("body serializes");
    let raw = shielded_transfer_message(body);
    let err = sentinel.on_data_received(raw).expect_err("missing spec fails closed");
    expect_validation_reason(err, ValidationFailureReason::UnsupportedAction);
    assert!(!registry.contains_shielded("tx-no-spec"));
}


/// The `"ShieldedTransfer"` inner-action arm of `on_data_received` (Phase
/// S3.2) routes to `handle_shielded_transfer`, which now runs the real
/// five-step path: with a stub spec registered, a well-formed body for a
/// token the provider does not know reaches the step-3 token gate and
/// fails closed with `Validation([TokenNotFound])` — not
/// `UnknownAction` (arm missing) and not the retired S3.2
/// `Routing("…not wired yet")` stub.
///
/// Discriminator: remove the arm ⇒ the same input surfaces
/// `UnknownAction("ShieldedTransfer")`; restore the stub ⇒ the old
/// `Routing` error.
#[test]
fn shielded_transfer_missing_token_fails_closed() {
    let (sentinel, registry) =
        make_sentinel_fixture_with_env_and_data_provider(StubDataProvider::new(), env_with_stub_shielded_spec());

    // The S3.2 fixture tx, verbatim (token_id vec![9, 10] — not in the
    // provider). The stub spec's validate_shielded passes (non-empty
    // proof), so the failure must be the token gate's.
    let body = serialize_to_bytes_rmp(&make_stub_shielded_tx("tx-shielded-arm")).expect("body serializes");
    let raw = shielded_transfer_message(body);

    let err = sentinel.on_data_received(raw).expect_err("unknown token fails closed");
    expect_validation_reason(err, ValidationFailureReason::TokenNotFound);
    assert!(!registry.contains_shielded("tx-shielded-arm"));
}


/// S5.1 step 3, gate 2: an unverified token is rejected with
/// `NotSelfVerified` before registration. (The fixture token IS
/// opt-in, so a missing self-verified gate would fall through to
/// registration + send and the no-registration assert below catches it.)
#[test]
fn shielded_transfer_not_self_verified_rejected() {
    let token = {
        let mut token = Token::new();
        token.is_self_verified = false;
        token.set_metadata("shielded_opt_in".to_string(), "true".to_string());
        token
    };
    let dp = StubDataProvider::new().with_token(vec![9, 10], "token".to_string(), token);
    let (sentinel, registry, finalizer_recorder, _exec, _identity) = make_shielded_fixture(
        dp,
        env_with_stub_shielded_spec(),
        Arc::new(SimpleShieldedPoolView::new(10)),
    );

    let body = serialize_to_bytes_rmp(&make_stub_shielded_tx("tx-not-self-verified")).expect("body serializes");
    let err = sentinel
        .on_data_received(shielded_transfer_message(body))
        .expect_err("not-self-verified fails closed");
    expect_validation_reason(err, ValidationFailureReason::NotSelfVerified);
    assert!(!registry.contains_shielded("tx-not-self-verified"));
    assert!(finalizer_recorder.lock().unwrap().is_empty(), "no send before registration");
}


/// S5.1 step 3, gate 3: a self-verified token that never opted in to
/// shielded transfer is rejected with `NotShieldedOptIn` before
/// registration.
#[test]
fn shielded_transfer_not_opt_in_rejected() {
    let token = {
        let mut token = Token::new();
        token.is_self_verified = true; // gate 2 passes…
        // …no "shielded_opt_in" metadata: gate 3 must fire.
        token
    };
    let dp = StubDataProvider::new().with_token(vec![9, 10], "token".to_string(), token);
    let (sentinel, registry, finalizer_recorder, _exec, _identity) = make_shielded_fixture(
        dp,
        env_with_stub_shielded_spec(),
        Arc::new(SimpleShieldedPoolView::new(10)),
    );

    let body = serialize_to_bytes_rmp(&make_stub_shielded_tx("tx-not-opt-in")).expect("body serializes");
    let err = sentinel
        .on_data_received(shielded_transfer_message(body))
        .expect_err("not opt-in fails closed");
    expect_validation_reason(err, ValidationFailureReason::NotShieldedOptIn);
    assert!(!registry.contains_shielded("tx-not-opt-in"));
    assert!(finalizer_recorder.lock().unwrap().is_empty(), "no send before registration");
}


/// S5.1 step 2 (advisory): a spec failure short-circuits **before** any
/// state is written — no registration, no finalizer work. The stub
/// rejects the empty proof with `InvalidShieldedProof`.
#[test]
fn shielded_transfer_advisory_failure_short_circuits_before_register() {
    let dp = StubDataProvider::new().with_token(vec![9, 10], "token".to_string(), make_opt_in_token());
    let (sentinel, registry, finalizer_recorder, _exec, _identity) = make_shielded_fixture(
        dp,
        env_with_stub_shielded_spec(),
        Arc::new(SimpleShieldedPoolView::new(10)),
    );

    let mut tx = make_stub_shielded_tx("tx-advisory-fail");
    tx.proof = vec![]; // the stub's advisory gate rejects this
    let body = serialize_to_bytes_rmp(&tx).expect("body serializes");
    let err = sentinel
        .on_data_received(shielded_transfer_message(body))
        .expect_err("advisory failure fails closed");
    expect_validation_reason(err, ValidationFailureReason::InvalidShieldedProof);
    assert!(!registry.contains_shielded("tx-advisory-fail"), "advisory failure registers nothing");
    assert!(finalizer_recorder.lock().unwrap().is_empty(), "advisory failure sends nothing");
}


/// S5.1 step 2 against the REAL spec (check 2): an already-spent
/// nullifier is rejected `StaleNullifier` by reading the shared pool
/// view — the seam the finalizer (S5.2) and committer (S5.3) consume.
/// The fixture tx decodes cleanly and its nullifier is marked spent on
/// the shared registry **before** the view is built, so the rejection
/// fires at check 2 — before check 4's lazy verifier (no keygen).
#[test]
fn shielded_transfer_stale_nullifier_rejected_via_pool_view() {
    let stx = make_shielded_tx_fixture("tx-stale-nullifier");

    let nullifiers = Arc::new(NullifierRegistry::new());
    nullifiers
        .try_mark_spent(stx.nullifiers[0])
        .expect("fresh registry: first mark succeeds");
    let roots = Arc::new(MerkleRootState::new(10)); // pristine — never reached (check 2 fires first)
    let view = Arc::new(SimpleShieldedPoolView::with(nullifiers, roots));

    let dp = StubDataProvider::new().with_token(stx.token_id.clone(), "token".to_string(), make_opt_in_token());
    let (sentinel, registry, finalizer_recorder, _exec, _identity) =
        make_shielded_fixture(dp, env_with_real_shielded_spec(), view);

    let body = serialize_to_bytes_rmp(&stx).expect("body serializes");
    let err = sentinel
        .on_data_received(shielded_transfer_message(body))
        .expect_err("spent nullifier fails closed");
    expect_validation_reason(err, ValidationFailureReason::StaleNullifier);
    assert!(!registry.contains_shielded("tx-stale-nullifier"));
    assert!(finalizer_recorder.lock().unwrap().is_empty());
}


/// S5.1 step 2 against the REAL spec (check 3): a referenced root beyond
/// the recency window is rejected `StaleMerkleRoot`. The shared root
/// state is advanced K+1=11 commits past genesis, so the tx's (fresh
/// tree's) root is absent/stale at check 3 — the fresh nullifier set
/// passes check 2 first, and the rejection lands before check 4's lazy
/// verifier (no keygen).
#[test]
fn shielded_transfer_stale_root_rejected_via_pool_view() {
    let stx = make_shielded_tx_fixture("tx-stale-root");

    // Distinct, decodable dummy roots (Fp::from(tag) in canonical form),
    // mirroring S4.3.3's concrete-type tests.
    let mut roots_state = MerkleRootState::new(10);
    for i in 1..=11u64 {
        roots_state.push(Fp::from(i).to_repr()); // tip at height 11: the tx's root is absent
    }
    let roots = Arc::new(roots_state);
    let nullifiers = Arc::new(NullifierRegistry::new()); // fresh: check 2 passes
    let view = Arc::new(SimpleShieldedPoolView::with(nullifiers, roots));

    let dp = StubDataProvider::new().with_token(stx.token_id.clone(), "token".to_string(), make_opt_in_token());
    let (sentinel, registry, finalizer_recorder, _exec, _identity) =
        make_shielded_fixture(dp, env_with_real_shielded_spec(), view);

    let body = serialize_to_bytes_rmp(&stx).expect("body serializes");
    let err = sentinel
        .on_data_received(shielded_transfer_message(body))
        .expect_err("stale root fails closed");
    expect_validation_reason(err, ValidationFailureReason::StaleMerkleRoot);
    assert!(!registry.contains_shielded("tx-stale-root"));
    assert!(finalizer_recorder.lock().unwrap().is_empty());
}


/// S5.1 step 4: the same id submitted twice — the atomic
/// `register_shielded` rejects the second with
/// `TransactionAlreadyExists`, and because registration precedes the
/// send, exactly one `SignShielded` ever goes out.
#[test]
fn shielded_transfer_duplicate_id_rejected_no_second_send() {
    let stx = make_stub_shielded_tx("tx-dup");
    let dp = StubDataProvider::new().with_token(stx.token_id.clone(), "token".to_string(), make_opt_in_token());
    let (sentinel, _registry, finalizer_recorder, _exec, _identity) = make_shielded_fixture(
        dp,
        env_with_stub_shielded_spec(),
        Arc::new(SimpleShieldedPoolView::new(10)),
    );

    let raw = shielded_transfer_message(serialize_to_bytes_rmp(&stx).expect("body serializes"));
    assert!(sentinel.on_data_received(raw.clone()).is_ok(), "first submission registers");

    match sentinel.on_data_received(raw) {
        Err(SentinelError::TransactionAlreadyExists(id)) => assert_eq!(id, "tx-dup"),
        other => panic!("expected TransactionAlreadyExists, got {other:?}"),
    }

    assert!(
        poll_recorder(&finalizer_recorder, 1),
        "the first submission's SignShielded reaches the finalizer"
    );
    assert_eq!(
        finalizer_recorder.lock().unwrap().len(),
        1,
        "exactly one outbound message: the duplicate produced none"
    );
}


/// S5.1 steps 4→5, the primary validation: a valid transfer reaches the
/// assigned finalizer as a signed `"SignShielded"` whose body is the
/// transaction's canonical bytes — and **never** touches an executor
/// (roadmap 2.1: shielded bypasses the Executor stage; no preload, no
/// executor dispatch).
#[test]
fn shielded_transfer_reaches_finalizer_without_executor() {
    let stx = make_stub_shielded_tx("tx-final");
    let dp = StubDataProvider::new().with_token(stx.token_id.clone(), "token".to_string(), make_opt_in_token());
    let (sentinel, registry, finalizer_recorder, executor_recorder, identity) = make_shielded_fixture(
        dp,
        env_with_stub_shielded_spec(),
        Arc::new(SimpleShieldedPoolView::new(10)),
    );

    let raw = shielded_transfer_message(serialize_to_bytes_rmp(&stx).expect("body serializes"));
    assert!(sentinel.on_data_received(raw).is_ok(), "valid shielded transfer passes end to end");

    // Step 4: registered in the parallel, never-evicted map.
    assert!(registry.contains_shielded("tx-final"));
    assert_eq!(
        registry.get_shielded("tx-final"),
        Some(stx.clone()),
        "the registered copy round-trips intact"
    );

    // Step 5: one signed SignShielded to the finalizer set, body = canonical bytes.
    assert!(
        poll_recorder(&finalizer_recorder, 1),
        "SignShielded reaches the finalizer"
    );
    let captured = finalizer_recorder.lock().unwrap()[0].clone();
    let message: Message = deserialize_rmp_to(&captured).expect("captured payload is a Message");
    assert_eq!(message.action, "SignShielded");
    assert_eq!(
        message.body,
        stx.canonical_bytes().expect("canonical bytes compute"),
        "the finalizer receives the canonical transaction bytes"
    );
    assert_signed_by(&message, &identity);

    // No executor dispatch at all (the finalizer's payload proves the
    // handler completed step 5, so any executor send would already be
    // in flight — its absence is final).
    assert!(
        executor_recorder.lock().unwrap().is_empty(),
        "shielded transfers never touch an executor"
    );
}


/// The ONE live proof through the full sentinel path. `#[ignore]`d per
/// the roadmap's "proving is benchmark-only" rule; run on demand with:
///
/// ```text
/// cargo test -p pneumatic_sentinel -- --ignored shielded_transfer_live_proof_end_to_end
/// ```
///
/// A real 1-in/1-out `build_shielded_tx` (S3.3), proved against the first
/// pool root a fresh network records: the genesis-seeded root state
/// advanced exactly one commit (the S4.3.4 bootstrap property — the
/// genesis seed is what makes check 3 well-defined on a fresh network).
/// Every gate — advisory spec (checks 1→4 with a REAL proof), token,
/// registration, deterministic assignment, signed `SignShielded` — runs
/// for real.
#[test]
#[ignore = "live halo2 prove (~1 min); see the test's doc comment"]
fn shielded_transfer_live_proof_end_to_end() {
    use pneumatic_prover::{build_shielded_tx, NoteOutput, ShieldedIdentity, SpendKey};

    // A satisfiable 1-in/1-out: value 100 = output 90 + fee 10, with the
    // input note a leaf of a fresh tree.
    let input_note = ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) };
    let spend_key = [0xABu8; 32];
    let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
    let (root, proof) = tree.append(&commit(&input_note));

    // The fresh network's pool state after that first commit: genesis
    // zero root (height 0) + the tree root (height 1) — the root the tx
    // references, so check 3 sees distance 0.
    let mut roots_state = MerkleRootState::new(10);
    roots_state.push(root_to_bytes(&root));
    let view = Arc::new(SimpleShieldedPoolView::with(
        Arc::new(NullifierRegistry::new()),
        Arc::new(roots_state),
    ));

    let recipient = ShieldedIdentity {
        spend: SpendKey::from_seed([2u8; 32]),
        identity: Ed25519Provider::generate(),
    };
    let (stx, _output_notes) = build_shielded_tx(
        &[7u8],
        &[input_note],
        &[&spend_key],
        &[proof],
        root_to_bytes(&root),
        &[NoteOutput::new(90, recipient)],
        10,
    )
    .expect("the real prove must succeed");

    let dp = StubDataProvider::new().with_token(stx.token_id.clone(), "token".to_string(), make_opt_in_token());
    let (sentinel, registry, finalizer_recorder, _exec, _identity) =
        make_shielded_fixture(dp, env_with_real_shielded_spec(), view);

    let raw = shielded_transfer_message(serialize_to_bytes_rmp(&stx).expect("body serializes"));
    assert!(sentinel.on_data_received(raw).is_ok(), "the live tx passes end to end");
    assert!(registry.contains_shielded(&stx.id), "registered in the parallel map");

    assert!(
        poll_recorder(&finalizer_recorder, 1),
        "SignShielded reaches the finalizer"
    );
    let captured = finalizer_recorder.lock().unwrap()[0].clone();
    let message: Message = deserialize_rmp_to(&captured).expect("captured payload is a Message");
    assert_eq!(message.action, "SignShielded");
    assert_eq!(message.body, stx.canonical_bytes().expect("canonical bytes compute"));

}
