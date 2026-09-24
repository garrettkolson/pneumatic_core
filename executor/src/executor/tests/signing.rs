//! Phase 1.1 regression: both `send_to_finalizer` paths sign `Execute`
//! with the executor's own identity — never the destination finalizer's key.
use super::helpers::*;
use super::super::*;

/// Both `send_to_finalizer` impls (Executor and ExecutorHandle — the one
/// spawned execution tasks use) must emit an "Execute" message signed
/// with the executor's identity, never the destination finalizer's key
/// (the pre-1.1 bug placed the destination key in the signature field).
#[tokio::test]
async fn send_to_finalizer_signed_with_executor_identity() {
    let identity = Arc::new(pneumatic_core::rns::identity::NodeIdentity::generate_in_memory());
    let node_registry = make_test_node_registry();
    // The finalizer peer's registered key.
    let finalizer_key = vec![0xAB; 32];
    let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(node_registry.register_peer(
        finalizer_key.clone(),
        [5u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(RecordingConnection { recorder: recorder.clone() }),
    ));

    let executor = Executor::new(
        "test_env".to_string(),
        vec![1, 2, 3, 4],
        identity.clone(),
        node_registry,
        make_test_data_provider(),
        make_test_pending_registry(),
        make_test_hash_provider(),
        10,
    );

    // Executor::send_to_finalizer ...
    executor
        .send_to_finalizer("test_tx_001", vec![1, 2, 3], vec![4, 5, 6])
        .await
        .expect("send should succeed");
    // ... and ExecutorHandle::send_to_finalizer (the impl used by run_execution).
    executor
        .clone_handle()
        .send_to_finalizer("test_tx_001", vec![7, 8, 9], vec![10, 11, 12])
        .await
        .expect("send should succeed");

    let captured = recorder.lock().unwrap();
    assert_eq!(captured.len(), 2, "finalizer should receive two Execute messages");
    for bytes in captured.iter() {
        let message: pneumatic_core::messages::Message =
            pneumatic_core::encoding::deserialize_rmp_to(bytes)
                .expect("captured payload should be a Message");
        assert_eq!(message.action, "Execute");
        assert_signed_by(&message, &identity);

        // ...and never under the destination's key.
        let verifier = pneumatic_core::crypto::Ed25519Provider::generate();
        let under_destination = verifier
            .check_signature(&message.signature, &finalizer_key, &message.body)
            .unwrap_or(false);
        assert!(
            !under_destination,
            "signature must not verify under the destination's key"
        );
    }
}
