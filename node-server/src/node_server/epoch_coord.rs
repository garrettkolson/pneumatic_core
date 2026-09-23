//! Epoch coordinator for the node-server: current-epoch observation,
//! roll-forward to the installed role-plugins, role-set recomputation on
//! an epoch boundary, and the coordinator tick (`poll_and_advance`).

use super::*;

impl NodeServer {
// --- Phase 5: epoch coordinator ----------------------------------------
// The installed role-plugins live in the `RoleDispatcher` as `Box<dyn
// RoleHost>`; the coordinator drives their lifecycle through the dispatcher.
// Every lifecycle method is `&self` and locks internally, so the spawned
// coordinator loop holds a single `Arc<Self>` and never takes a `&mut`
// (the boxed `RoleHost` gives the Finalizer's `&mut self` its mutable handle
// via the dispatcher's own `iter_mut` — no Mutex around the plugins).

/// The epoch number the shared `EpochBoundaryDetector` currently holds — the
/// epoch the Committer has last advanced it to (the Committer is the single
/// writer; the coordinator only reads). Exposed so tests and the lifecycle
/// coordinator can observe the current epoch.
pub fn current_epoch(&self) -> u64 {
    self.epoch_boundary_detector.current_epoch.epoch_number
}

/// Fan one epoch advance to every installed role (Phase 5, `advance_epoch`
/// fan-out on an epoch boundary). Delegates the fan-out to the dispatcher,
/// which visits every installed host (the Committer's `advance_epoch` is a
/// no-op that self-drives) and returns the roles visited.
pub async fn roll_forward(
    &self,
    epoch: u64,
) -> Vec<pneumatic_core::node::NodeRegistryType> {
    let mut guard = self.role_dispatcher.lock().await;
    guard.roll_forward(epoch)
}

/// Recompute the role set this node qualifies for by stake, on an epoch
/// boundary — the selection is re-evaluated because stake can change per
/// epoch. Equivalent to a fresh `select()`; tracked by the coordinator so a
/// changed set can trigger re-registration (Phase 6).
pub fn recompute_role_set(&self) -> Vec<pneumatic_core::node::NodeRegistryType> {
    self.role_selector.lock().unwrap().select()
}

/// The coordinator's single epoch tick: when the shared detector reports the
/// current epoch expired at `now`, advance — refresh the registration gate's
/// off-thread index via `StakeIndex::set_epoch`, fan `advance_epoch` to the
/// installed roles, and recompute the role set — and return `true`;
/// otherwise return `false` (no advance this tick). No sleep; the spawned
/// loop (`spawn_coordinator`) owns the cadence.
pub async fn poll_and_advance(&self, now: i64) -> bool {
    if !self.epoch_boundary_detector.is_epoch_expired(now) {
        return false;
    }
    let epoch = self.current_epoch();
    self.stake_index.set_epoch(epoch);
    self.roll_forward(epoch).await;
    self.recompute_role_set();
    true
}
}
