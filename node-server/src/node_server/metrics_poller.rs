//! The metrics poller: the composite's read-only host state exported as
//! Prometheus gauges on a real cadence.
//!
//! Why this is a module instead of the loop the binary once inlined: the
//! inlined loop ended its iteration with `shutdown_rx.changed().await` — and
//! a `tokio::sync::watch` receiver fires only when a value is *sent*, which
//! first happens at shutdown. The loop therefore ran its gauge pass exactly
//! once, at boot, and then parked: `pneumatic_node_peers` froze at its
//! boot-time zero forever while the directories behind it filled
//! (discovered 10/08/2026 in the multi-host rehearsal, where the mesh formed
//! and the fragments proved the directories full while every node kept
//! advertising 0 peers). A metrics poller that polls once is worse than no
//! poller: it advertises a healthy-looking lie indefinitely. The cadence is
//! now an argument, and the regression is pinned by
//! `metrics_poller_refreshes_gauges_after_boot`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::telemetry::Metrics;

use super::NodeServer;

impl NodeServer {
    /// Poll this host's read-only state into `metrics` every `interval`,
    /// exiting when `shutdown` fires (value sent) or is dropped. The first
    /// pass runs immediately so a scrape before the first tick still sees
    /// the boot state.
    pub fn spawn_metrics_poller(
        self: &Arc<Self>,
        metrics: Arc<Metrics>,
        mut shutdown: watch::Receiver<bool>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let host = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                host.poll_metrics(&metrics);
                tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        // Sender dropped: the process is unwinding.
                        // Value sent (true): operator requested shutdown.
                        if changed.is_err() || *shutdown.borrow_and_update() {
                            break;
                        }
                    }
                    // The tick the inline loop never had: without it
                    // `changed()` parks the loop forever and every gauge
                    // freezes at its boot-time value.
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        })
    }

    /// One read-only pass: every gauge the host can answer without I/O
    /// beyond in-process atomics and DashMap reads.
    fn poll_metrics(&self, metrics: &Metrics) {
        metrics.set_gauge("pneumatic_epoch_current", self.current_epoch());
        metrics.set_gauge(
            "pneumatic_installed_roles",
            self.installed_roles().len() as u64,
        );
        let mut peers = 0u64;
        for node_type in [
            NodeRegistryType::Committer,
            NodeRegistryType::Sentinel,
            NodeRegistryType::Executor,
            NodeRegistryType::Finalizer,
            NodeRegistryType::Archiver,
        ] {
            if let Some(nodes) = self.node_registry().get_nodes(&node_type) {
                peers += nodes.len() as u64;
            }
        }
        metrics.set_gauge("pneumatic_node_peers", peers);
        metrics.set_gauge("pneumatic_tokens_cached", self.tokens().len() as u64);
        metrics.set_gauge(
            "pneumatic_pending_transactions",
            self.pending_registry().in_flight_count() as u64,
        );
        metrics.set_gauge(
            "pneumatic_shielded_transactions",
            self.pending_registry().shielded_count() as u64,
        );
        metrics.set_gauge("pneumatic_up", 1);
        // Phase 2 resource metric: the process-global verification tally from
        // the crypto chokepoint (`pneumatic_core::crypto::signature_verifications`).
        // A monotonic total exported as a gauge — the sampler differences two
        // reads for verifications/s.
        metrics.set_gauge(
            "pneumatic_signature_verifications_total",
            pneumatic_core::crypto::signature_verifications(),
        );
    }
}
