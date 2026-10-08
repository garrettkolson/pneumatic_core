//! Metrics-poller cadence regressions.
//!
//! The multi-host rehearsal (10/08/2026) caught the previous poller reporting
//! `pneumatic_node_peers 0` on a fully peered cluster: its loop body ran
//! exactly once at boot because `shutdown_rx.changed()` never ticks. These
//! tests pin the fix — the gauge must reflect registrations that happen
//! AFTER startup, and the loop must still exit on shutdown.

use std::sync::Arc;
use std::time::Duration;

use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::telemetry::Metrics;

use super::helpers::*;
use super::super::*;

/// A peer that registers after boot must move `pneumatic_node_peers`.
/// Against the old one-pass loop this fails: the boot-time gauge (0) is the
/// only value the poller ever exports, no matter what the directories fill
/// in afterward.
#[tokio::test]
async fn metrics_poller_refreshes_gauges_after_boot() {
    let cfg = runtime_config(vec![bad_peer()], type_config_floor(0));
    let server = Arc::new(
        build_runtime(
            cfg,
            Arc::new(MapStakeProvider::with_default(2000)),
            test_data_provider(),
        )
        .expect("host boots without transport"),
    );
    let metrics = Arc::new(Metrics::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let poller =
        server.spawn_metrics_poller(metrics.clone(), shutdown_rx, Duration::from_millis(20));

    // Let the first ticks definitely land: boot state is zero peers.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        metrics.get("pneumatic_node_peers"),
        Some(0),
        "the boot-time pass exports the empty directory"
    );

    // Register a peer AFTER startup — the exact event the frozen loop missed.
    assert!(
        server.node_registry().register_peer(
            vec![0xAA; 32],
            [7u8; 16],
            &NodeRegistryType::Sentinel,
            Box::new(RecordingConnection {
                recorder: Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
        ),
        "the test peer registers"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if metrics.get("pneumatic_node_peers") == Some(1) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pneumatic_node_peers never refreshed after the registration — \
             the poller is parked on a one-pass boot gauge (the rehearsal bug)"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Shutdown releases the loop (a poller that never exits leaks a task).
    let _ = shutdown_tx.send(true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(poller.is_finished(), "the poller exits when shutdown fires");
}
