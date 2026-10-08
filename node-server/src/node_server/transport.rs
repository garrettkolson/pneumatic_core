//! Transport + lifecycle for the node-server: the RNS data-plane
//! routing unit (`route_data_plane`) and the lifecycle fan-outs
//! (graceful shutdown to all plugins, spawned coordinator loop).

use super::*;

impl NodeServer {
/// Fan graceful shutdown to every installed role (Committer + Finalizer run
/// real shutdown; Sentinel + Executor no-op).
pub async fn initiate_all_shutdown(&self) {
    let mut guard = self.role_dispatcher.lock().await;
    guard.initiate_all_shutdown().await;
}

/// Spawn the epoch-coordinator background loop: poll `poll_and_advance`
/// every `interval_ms` until shutdown. The loop is thin glue (poll + sleep);
/// `poll_and_advance` carries the substantive advance logic and is covered by
/// the phase discriminators. The Committer's own `run_epoch_loop` (which
/// advances the shared detector this loop watches) is spawned by the caller
/// / Phase 7 end-to-end wiring.
pub fn spawn_coordinator(self: Arc<Self>, interval_ms: u64) {
    tokio::spawn(async move {
        loop {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            self.poll_and_advance(now).await;
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        }
    });
}
}

/// Route one RNS data-plane payload to the installed role that owns its action.
///
/// Phase 7: this is the extractable unit of the transport bridge. The RNS
/// `on_packet` handler (see `build_runtime`) parses each inbound frame into a
/// `NetworkPacket` and hands the data-plane bytes to this function; a reverted
/// bridge that drops the payload or never routes to the dispatcher fails the
/// `inbound_data_packet_routes_by_bridge` discriminator. Fail-closed: a payload
/// that does not parse as a `Message` (or whose action no installed role owns)
/// is surfaced, never silently swallowed.
pub(crate) async fn route_data_plane(
    data: Vec<u8>,
    dispatcher: Arc<TokioMutex<RoleDispatcher>>,
) -> Result<(), RoleError> {
    let message = pneumatic_core::encoding::deserialize_rmp_to::<pneumatic_core::messages::Message>(&data)
        .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("undecodable data-plane message: {e}"))))?;
    dispatcher.lock().await.dispatch(message).await
}

/// Run [`route_data_plane`] on the host runtime, never the caller's ambient
/// context — the ONLY correct shape for the RNS bridge's data-plane branch.
///
/// The `on_packet` callback runs on RNS's own inbound **worker threads**:
/// plain `std::thread`s with no ambient reactor. There, a bare `tokio::spawn`
/// does not merely fail the frame — it **panics** ("there is no reactor
/// running") and kills the worker: its inbound queue's receiving half drops,
/// every subsequent frame logs "inbound queue closed; dropping", and within
/// one liveness-eviction window the node's directories empty itself —
/// `pneumatic_node_peers` 12 → 0, mesh gone. Measured 10/08/2026 in the
/// multi-host rehearsal: the first data-plane frame of a 200-tx burst killed
/// one worker on every receiving peer; nothing upstream looked wrong from the
/// sender's side (fan-out swallows RNS send results). A `Handle` captured
/// inside the host runtime spawns correctly from any thread.
pub(crate) fn spawn_data_dispatch(
    rt: &tokio::runtime::Handle,
    dispatcher: Arc<TokioMutex<RoleDispatcher>>,
    data: Vec<u8>,
) {
    rt.spawn(async move {
        if let Err(e) = route_data_plane(data, dispatcher).await {
            // Same promise as `route_data_plane`'s doc: surfaced, never
            // silently swallowed. (The bridge previously dropped the Result.)
            eprintln!("[pneumatic] data-plane dispatch failed: {e}");
        }
    });
}
