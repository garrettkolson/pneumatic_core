//! Fan-out tests: `bounded_send(_async)` timeout arms, failure recording
//! by (rhash, type), blocking-direct delivery (real sends, timeouts),
//! `send_to_all` self-inclusion, and success counters.
use super::helpers::*;
use super::super::*;

// --- Phase 6.1: per-send blocking I/O timeouts (H7) ---
//
// The RNS fan-out can't be driven through the concrete `RnsNetwork` in a
// unit test (tests use `network: None`), so these test the bounding
// primitives the two fan-out methods delegate to. On revert (no bound) the
// slow-closure discriminators hang the suite instead of timing out.

/// Positive control: a fast closure returns its result promptly.
#[test]
fn bounded_send_fast_returns_ok() {
    let start = Instant::now();
    let result = bounded_send(Duration::from_secs(10), || vec![1u8, 2u8, 3u8]);
    assert_eq!(result.unwrap(), vec![1u8, 2u8, 3u8]);
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "fast send should return near-instantly, took {:?}",
        start.elapsed()
    );
}

/// Discriminator (sync): a slow closure times out instead of hanging.
#[test]
fn bounded_send_slow_times_out() {
    let start = Instant::now();
    let result: Result<Vec<u8>, ConnError> = bounded_send(Duration::from_millis(100), || {
        std::thread::sleep(Duration::from_millis(300));
        Vec::<u8>::new()
    });
    assert!(
        matches!(result, Err(ConnError::Timeout(_))),
        "expected a timeout, got {result:?}"
    );
    assert!(
        start.elapsed() <= Duration::from_millis(100) + Duration::from_millis(100),
        "timed out too late: took {:?}",
        start.elapsed()
    );
}

/// Positive control (async): a fast closure returns promptly.
#[tokio::test]
async fn bounded_send_async_fast_returns_ok() {
    let start = Instant::now();
    let result = bounded_send_async(Duration::from_secs(10), || vec![0u8]).await;
    assert_eq!(result.unwrap(), vec![0u8]);
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "fast async send should return near-instantly, took {:?}",
        start.elapsed()
    );
}

/// Discriminator (async): a slow closure is cancelled at the bound rather
/// than hanging the caller. On revert this hangs.
#[tokio::test]
async fn bounded_send_async_slow_times_out() {
    let start = Instant::now();
    let result: Result<Vec<u8>, ConnError> =
        bounded_send_async(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_millis(300));
            Vec::<u8>::new()
        })
        .await;
    assert!(
        matches!(result, Err(ConnError::Timeout(_))),
        "expected a timeout, got {result:?}"
    );
    assert!(
        start.elapsed() <= Duration::from_millis(100) + Duration::from_millis(100),
        "timed out too late: took {:?}",
        start.elapsed()
    );
}

/// Discriminator (async direct, `Ok(Err)` arm): three failing peers each
/// record exactly one failure, keyed by (rhash, type). On revert (`let _ =`)
/// nothing is recorded and the counters stay 0.
#[tokio::test]
async fn direct_delivery_failure_is_recorded() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
    for (i, rhash) in [(1u8, [1u8; 16]), (2, [2u8; 16]), (3, [3u8; 16])] {
        reg.register_peer(
            vec![i],
            rhash,
            &NodeRegistryType::Finalizer,
            Box::new(FailingConnection),
        );
    }
    reg.send_to_all(vec![9u8], &NodeRegistryType::Finalizer).await;
    assert_eq!(reg.total_delivery_failures(), 3);
    for rhash in [[1u8; 16], [2u8; 16], [3u8; 16]] {
        assert_eq!(
            reg.failure_count(rhash, &NodeRegistryType::Finalizer),
            1,
            "each peer must record exactly one failure"
        );
    }
}

/// Discriminator (composite loopback assumption): `send_to_all` reaches a
/// node's own connection in its own bucket — there is no self-skip. The
/// composite node-server relies on this so cross-role messaging (e.g.
/// Executor→Finalizer via `send_to_all(&Finalizer)`) loops back over RNS to
/// the same process and is routed to the target role. On a revert that
/// filters the owning key out of the fan-out, nothing arrives on the channel.
#[tokio::test]
async fn send_to_all_includes_self() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Committer, 5)]);
    let (tx, mut rx) = mpsc::channel(16);
    // Register THIS node's own key (its real config public key) under its
    // own bucket with a spy connection — the way a composite registers
    // itself in every selected bucket.
    let own_key = reg.get_config().public_key.clone();
    reg.register_peer(
        own_key,
        [7u8; 16],
        &NodeRegistryType::Committer,
        Box::new(RecordingConnection { tx }),
    );
    reg.send_to_all(vec![5u8], &NodeRegistryType::Committer).await;
    assert_eq!(
        rx.try_recv().expect("own bucket must receive the payload"),
        vec![5u8],
        "send_to_all must not skip the node's own connection"
    );
}

/// Discriminator (blocking direct): the rewritten branch drives the async
/// `send` on a local runtime instead of dropping the future, so the peer
/// actually receives the payload. On revert (future dropped un-awaited)
/// nothing arrives on the channel and this panics.
#[test]
fn blocking_direct_actually_sends_data() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
    let (tx, mut rx) = mpsc::channel(16);
    let payload = vec![42u8, 43, 44];
    reg.register_peer(
        vec![1],
        [1u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(RecordingConnection { tx }),
    );
    reg.send_to_all_blocking(payload.clone(), &NodeRegistryType::Finalizer);
    assert_eq!(
        rx.try_recv().expect("peer should have received the payload"),
        payload,
        "blocking direct branch must actually send"
    );
}

/// Discriminator (blocking direct, `Ok(Err)` arm): a failing peer's failure
/// is recorded. On revert the `let _ =` swallows it and the counter stays 0.
#[test]
fn blocking_direct_delivery_failure_is_recorded() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
    reg.register_peer(
        vec![1],
        [1u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(FailingConnection),
    );
    reg.send_to_all_blocking(vec![9u8], &NodeRegistryType::Finalizer);
    assert_eq!(
        reg.failure_count([1u8; 16], &NodeRegistryType::Finalizer),
        1,
        "blocking direct branch must record the failure"
    );
}

/// Discriminator (async direct: both `Ok(Err)` and `Err(Elapsed)` arms): a
/// failing peer records on the `Ok(Err)` arm, a hanging peer records on the
/// elapsed-timeout arm. On revert the elapsed peer is never recorded.
#[tokio::test]
async fn direct_send_timeout_is_recorded_as_failure() {
    let mut reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
    reg.with_send_timeout(Duration::from_millis(50));
    reg.register_peer(
        vec![1],
        [1u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(FailingConnection),
    );
    reg.register_peer(
        vec![2],
        [2u8; 16],
        &NodeRegistryType::Finalizer,
        Box::new(HangingConnection {
            dur: Duration::from_secs(5),
        }),
    );
    reg.send_to_all(vec![9u8], &NodeRegistryType::Finalizer).await;
    assert_eq!(
        reg.failure_count([1u8; 16], &NodeRegistryType::Finalizer),
        1,
        "immediate send error (Ok(Err)) must be recorded"
    );
    assert_eq!(
        reg.failure_count([2u8; 16], &NodeRegistryType::Finalizer),
        1,
        "elapsed timeout (Err(Elapsed)) must be recorded"
    );
}

/// Positive control: a successful fan-out records no failures, so the
/// counter cannot grow merely by sending.
#[tokio::test]
async fn successful_delivery_records_no_failure() {
    let reg = registry_with_capacity(&[(NodeRegistryType::Finalizer, 5)]);
    for (i, rhash) in [(1u8, [1u8; 16]), (2, [2u8; 16])] {
        reg.register_peer(
            vec![i],
            rhash,
            &NodeRegistryType::Finalizer,
            Box::new(NullConnection),
        );
    }
    reg.send_to_all(vec![9u8], &NodeRegistryType::Finalizer).await;
    assert_eq!(
        reg.total_delivery_failures(),
        0,
        "successful deliveries must not record failures"
    );
}

/// Discriminator of the helper itself: failures accumulate per (rhash,
/// type). A no-op helper reverts to all-zero.
#[test]
fn record_delivery_failure_counts_by_rhash_and_type() {
    let failures = Arc::new(DashMap::new());
    let ft = NodeRegistryType::Finalizer;
    record_delivery_failure(&failures, [1u8; 16], &ft, ConnError::IO("a".into()));
    record_delivery_failure(
        &failures,
        [1u8; 16],
        &ft,
        ConnError::WriteError(Some("b".into())),
    );
    record_delivery_failure(
        &failures,
        [2u8; 16],
        &ft,
        ConnError::Timeout("c".into()),
    );
    assert_eq!(
        failures
            .get(&([1u8; 16], ft.clone()))
            .map(|c| *c.value())
            .unwrap_or(0),
        2
    );
    assert_eq!(
        failures
            .get(&([2u8; 16], ft.clone()))
            .map(|c| *c.value())
            .unwrap_or(0),
        1
    );
    assert_eq!(
        failures.iter().map(|e| *e.value()).sum::<u64>(),
        3,
        "total must be the sum across keys"
    );
}
