//! Shielded dispatch tests: `SignShielded` reaches the finalizer
//! plugin's auth gate; removing it from the reported action set is
//! rejected at the dispatcher (the gate is load-bearing).
use super::helpers::*;
use super::super::*;

/// The `"SignShielded"` vote request reaches the installed Finalizer's real
/// plugin handler (`handle_sign_shielded`) — not `UnknownAction` (action not
/// registered in `FINALIZER_ACTIONS`) and not the else-fail-closed arm
/// ("unhandled inbound action"). S5.2: the real handler's first gate is
/// envelope authentication, so an unsigned message fails closed there with
/// the distinct "envelope signature verification failed" crypto error,
/// proving the message traversed dispatcher → plugin → match arm. Reverted
/// (arm or `FINALIZER_ACTIONS` entry removed) this surfaces `UnknownAction`
/// instead.
#[tokio::test]
async fn signshielded_request_reaches_finalizer_plugin_auth_gate() {
    let cfg = runtime_config(vec![bad_peer()], type_config_select(NodeRegistryType::Finalizer));
    let provider = Arc::new(MapStakeProvider::with_default(2000));
    let server = build_runtime(cfg, provider, test_data_provider()).expect("host builds");

    assert_eq!(server.installed_roles(), vec![NodeRegistryType::Finalizer]);

    // A wire-serializable `ShieldedTransaction` body — an empty body would
    // only prove the `Encoding` path; this proves the *arm* ran and reached
    // the auth gate before deserialization.
    let shielded_tx = ShieldedTransaction {
        id: "tx-shielded-1".to_string(),
        action: "ShieldedTransfer".to_string(),
        token_id: vec![1, 2, 3],
        spent_commitments: vec![[0u8; 32]],
        nullifiers: vec![[0u8; 32]],
        commitments: vec![[1u8; 32]],
        merkle_root: [2u8; 32],
        proof: vec![9, 9, 9],
        note_ciphertexts: vec![vec![7u8; 64]],
        fee: 0,
    };
    let body = serialize_to_bytes_rmp(&shielded_tx).expect("body serializes");
    let message = Message {
        chain_id: "env".to_string(),
        action: "SignShielded".to_string(),
        body,
        signature: vec![],
        public_key: vec![],
        stake_set: None,
    };

    match server.dispatch(message).await {
        // Routing reached the real handler, which fails closed at envelope
        // authentication. The dispatcher wraps the finalizer's `PneumaticError`
        // in `Network(«{e:?}»)` — the inner `Crypto` variant text must be
        // present, and no routing failure.
        Err(RoleError::Downstream(e)) => {
            let s = e.to_string();
            assert!(
                s.contains("envelope signature verification failed"),
                "expected the real handler's fail-closed auth gate error, got {s:?}"
            );
            assert!(
                !s.contains("UnknownAction") && !s.contains("unhandled inbound action"),
                "must route to the SignShielded arm, not reject at the dispatcher: {s:?}"
            );
        }
        other => panic!("expected the fail-closed Downstream auth error, got {other:?}"),
    }
}

/// S5.4 discriminator: the `"SignShielded"` entry in `FINALIZER_ACTIONS`
/// is the routing gate, not decoration. The real finalizer plugin (the
/// same construction the composite's finalizer arm performs, over the
/// shared live pool) is presented to a dispatcher under a reduced action
/// set — `"SignShielded"` removed — and a `SignShielded` vote request
/// must be rejected `UnknownAction` at the dispatcher, BEFORE any handler
/// work. If the gate were not load-bearing (routing by role type rather
/// than the reported action set), this test dispatches into the loud
/// failure arm instead. The positive-side twin is
/// `signshielded_request_reaches_finalizer_plugin_auth_gate` (entry
/// present ⇒ routed to the real handler).
#[tokio::test]
async fn signshielded_removed_from_action_set_is_rejected_by_dispatcher() {
    let config = runtime_config(
        vec![bad_peer()],
        type_config_select(NodeRegistryType::Finalizer),
    );
    let env_data = config
        .environment_metadata
        .get(&config.main_environment_id)
        .map(|e| Arc::new(e.value().clone()))
        .expect("test config carries its main environment");

    let data_provider: Arc<dyn DataProvider> = Arc::new(DefaultDataProvider::new());
    let hash_provider: Arc<dyn HashProvider> = Arc::new(BasicHashProvider::new());
    let node_registry = Arc::new(NodeRegistry::init(
        config.clone(),
        None,
        StakeIndex::new(
            data_provider.clone(),
            env_data.token_partition_id.clone(),
            1,
            None,
        )
        .make_check(config.clone()),
    ));
    let pending_registry = Arc::new(PendingTransactionRegistry::new());

    // The composite finalizer arm's construction (S5.4: the view is the
    // shared live pool, erased to the seam — exactly the arm's in-fn
    // derivation).
    let verifying_key: VerifyingKey = {
        let pk = config.public_key.clone();
        let pk_bytes: [u8; 32] = pk.try_into().expect("32-byte pk");
        VerifyingKey::from_bytes(&pk_bytes).expect("valid vk")
    };
    let signature_registry = Arc::new(TransactionSignatureRegistry::new());
    let shielded_pool =
        Arc::new(pneumatic_committer::shielded_pool::ShieldedPool::new(10));
    let shielded_pool_view: Arc<dyn pneumatic_core::shielded::ShieldedPoolView> =
        shielded_pool.clone() as Arc<dyn pneumatic_core::shielded::ShieldedPoolView>;
    let finalizer = pneumatic_finalizer::Finalizer::new(
        env_data.environment_id.clone(),
        config.public_key.clone(),
        config.identity.clone(),
        node_registry,
        pending_registry,
        signature_registry,
        66.6,
        4,
        config.identity.clone(),
        verifying_key,
        hash_provider,
        vec![],
        0,
        vec![],
        1,
        data_provider,
        env_data.token_partition_id.clone(),
        shielded_pool_view,
        env_data.clone(),
    );

    let plugin: Box<dyn RoleHost> = Box::new(ReducedFinalizerPlugin(finalizer));
    let dispatcher = RoleDispatcher::new(vec![plugin]);

    // The vote request (the same action the composite's sentinel emits).
    // An empty body is deliberately fine: the dispatcher inspects only
    // the action string, so this proves the gate fired pre-body.
    match dispatcher.dispatch(msg("SignShielded")).await {
        Err(RoleError::UnknownAction(action)) => assert_eq!(action, "SignShielded"),
        other => panic!(
            "expected the dispatcher's UnknownAction for the removed entry, got {other:?}"
        ),
    }
}
