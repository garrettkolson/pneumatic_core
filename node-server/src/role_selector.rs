//! Role-selection-by-stake — the headline new behavior. Choses which
//! role-plugins a node installs from its own stake against the protocol +
//! per-type floors. Implemented in Phase 1.
//!
//! The selector reuses `meets_minimum_stake` (`pneumatic_core::config`) — the
//! AND-of-two-floors primitive the registration gate already enforces — so
//! "how much stake does a key need" has a single source of truth, exactly like
//! the `ActionRouter::check_stake` path (action_router.rs:186) and the
//! `StakeIndex` registration gate. It is a fresh in-process layer, never the
//! dead `pneumatic_core::server::ThreadPool`.
//!
//! Own stake is *fail-closed*: any cache miss reports `0` ⇒ the node qualifies
//! for no role ⇒ it installs nothing. The stake value is looked up once per
//! `select()` (it does not depend on role) and reused across every floor check,
//! so the selection path does no per-type I/O.

use std::sync::Arc;

use pneumatic_core::config::{meets_minimum_stake, Config};
use pneumatic_core::node::NodeRegistryType;
use strum::IntoEnumIterator;

/// Source of the node's own stake at a given epoch, for the selection path.
///
/// Decouples `RoleSelector` from the concrete `StakeIndex` so the selection
/// logic can be unit-tested against an in-memory map; the real wiring
/// (Phase 3) hands it a provider backed by the *same* map as the registration
/// gate — single data-service path, no I/O on the selection path beyond one
/// in-process read. Fail-closed: a miss (or an unset / zero key) reports `0`.
pub trait StakeProvider: Send + Sync {
    /// Own stake for `public_key` at `epoch`, in the node's partition. Returns
    /// `0` on any miss so selection fails closed rather than installing a role
    /// the node's stake cannot back.
    fn stake(&self, public_key: &[u8], epoch: u64) -> u64;
}

/// Chooses which role-plugins a node installs from its own stake against the
/// protocol + per-type floors.
///
/// The full qualifying set (`select`) is iterated in `NodeRegistryType` order
/// and each type is admitted only when the node's own stake meets *both* the
/// global and the per-type floor. For the Phase-1 bootstrap (single-role
/// identity, low-risk) `select_primary` returns the one highest-priority
/// qualifying role, in `Finalizer > Executor > Sentinel > Committer` order —
/// the same priority `NodeRegistry::select_registration_node_type` uses.
pub struct RoleSelector {
    config: Arc<Config>,
    stake: Arc<dyn StakeProvider>,
    epoch: u64,
    last_selected: Vec<NodeRegistryType>,
}

/// The priority the composite host resolves to when it can only run one role
/// for this identity (Phase-1 bootstrap). Matches `NodeRegistry`'s own
/// `select_registration_node_type` so a node's chosen role is the same one the
/// registration gate would accept it for.
const ROLE_PRIORITY: [NodeRegistryType; 4] = [
    NodeRegistryType::Finalizer,
    NodeRegistryType::Executor,
    NodeRegistryType::Sentinel,
    NodeRegistryType::Committer,
];

impl RoleSelector {
    /// Build a selector over `config`'s floors against `stake`. Boot epoch is 1
    /// (the `StakeIndex`'s boot epoch), matching the registration gate's start.
    pub fn new(config: Arc<Config>, stake: Arc<dyn StakeProvider>) -> Self {
        Self { config, stake, epoch: 1, last_selected: Vec::new() }
    }

    /// The epoch this selector is currently evaluating stake at. The lifecycle
    /// loop calls `set_epoch` on each advance so the next `select()` re-reads
    /// the updated stake snapshot.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Advance to `epoch` so the next `select()` re-evaluates stake at the new
    /// epoch (stake can change per epoch, so the role set can change too).
    /// Does not itself recompute — the coordinator calls `select()` after.
    pub fn set_epoch(&mut self, epoch: u64) -> &mut Self {
        self.epoch = epoch;
        self
    }

    /// The full set of roles the node qualifies for right now (own stake meets
    /// both floors for each), in `NodeRegistryType` order. Recomputes from the
    /// current epoch. Called at boot and on each epoch advance — never on the
    /// hot path.
    pub fn select(&mut self) -> Vec<NodeRegistryType> {
        // Own stake does not depend on role: read it once and reuse it across
        // every floor check (single source of truth, no per-type I/O).
        let own_stake = self.stake.stake(&self.config.public_key, self.epoch);
        let global_min = self.config.get_global_min_stake();
        let qualifying: Vec<NodeRegistryType> = NodeRegistryType::iter()
            .filter(|t| {
                meets_minimum_stake(
                    own_stake,
                    global_min,
                    self.config.get_min_type_stake(t),
                )
            })
            .collect();
        self.last_selected = qualifying.clone();
        qualifying
    }

    /// The single highest-priority qualifying role (`Finalizer > Executor >
    /// Sentinel > Committer`), or `None` if the node qualifies for no role.
    /// Phase-1 bootstrap installs this one role for a composite identity; the
    /// Phase-6 path uses `select()` in full.
    pub fn select_primary(&self) -> Option<NodeRegistryType> {
        ROLE_PRIORITY
            .iter()
            .find(|t| meets_minimum_stake(self.own_stake(), self.config.get_global_min_stake(), self.config.get_min_type_stake(t)))
            .cloned()
    }

    /// The set returned by the most recent `select()`, for diffing against a
    /// recompute after an epoch advance.
    pub fn selected_roles(&self) -> &[NodeRegistryType] {
        &self.last_selected
    }

    fn own_stake(&self) -> u64 {
        self.stake.stake(&self.config.public_key, self.epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_helpers::*;
    use strum::IntoEnumIterator;
    use std::sync::atomic::Ordering;

    #[test]
    fn fails_closed_on_zero_stake() {
        let config = config_with_type_floor(10);
        // No epoch overrides; default stake is 0 => everything misses.
        let stake = Arc::new(MapStakeProvider::with_default(0));
        let mut sel = RoleSelector::new(config, stake);
        let roles = sel.select();
        assert!(roles.is_empty(), "zero/unset stake must qualify for no role, got {roles:?}");
    }

    #[test]
    fn requires_both_global_and_type_floor() {
        let config = config_with_type_floor(1000); // type floor >> global floor (10)

        // Above global (10) but below type (1000): the AND still rejects.
        {
            let mut stake = MapStakeProvider::with_default(0);
            stake.with_epoch(1, 100);
            let mut sel = RoleSelector::new(config.clone(), Arc::new(stake));
            assert!(sel.select().is_empty(), "meeting only the global floor must not admit a role");
        }

        // Above both floors: admitted.
        {
            let mut stake = MapStakeProvider::with_default(0);
            stake.with_epoch(1, 2000);
            let mut sel = RoleSelector::new(config.clone(), Arc::new(stake));
            let roles = sel.select();
            assert_eq!(roles.len(), NodeRegistryType::iter().count(), "stake over both floors qualifies for every role");
        }
    }

    #[test]
    fn reevaluates_on_epoch_advance() {
        let config = config_with_type_floor(1000);
        let mut stake = MapStakeProvider::with_default(0);
        stake.with_epoch(1, 2000); // qualifies at epoch 1
        stake.with_epoch(2, 0); // nothing at epoch 2
        let mut sel = RoleSelector::new(config, Arc::new(stake));

        assert_eq!(sel.select().len(), NodeRegistryType::iter().count(), "qualifies at epoch 1");

        sel.set_epoch(2).select();
        assert!(
            sel.select().is_empty(),
            "a stake drop at the new epoch must drop every role"
        );
        assert_eq!(sel.epoch(), 2);
    }

    #[test]
    fn select_reads_stake_once_per_call() {
        let config = config_with_type_floor(10);
        let provider = Arc::new(MapStakeProvider::with_default(2000)); // over all floors
        let mut sel = RoleSelector::new(config, provider.clone());

        let roles = sel.select();
        assert_eq!(roles.len(), NodeRegistryType::iter().count());
        // Own stake is looked up exactly once for the whole selection, not once
        // per role type: single source of truth, no per-type lookups.
        assert_eq!(
            provider.lookups.load(Ordering::SeqCst),
            1,
            "select() must read own stake once, reused across all floor checks"
        );
    }

    #[test]
    fn select_primary_picks_highest_priority_qualifying_role() {
        let config = config_with_type_floor(10);
        let mut stake = MapStakeProvider::with_default(0);
        stake.with_epoch(1, 2000); // qualifies for all roles
        let mut sel = RoleSelector::new(config.clone(), Arc::new(stake));
        sel.select();

        assert_eq!(
            sel.select_primary(),
            Some(NodeRegistryType::Finalizer),
            "with all roles qualifying, the highest-priority (Finalizer) wins"
        );

        // Below floors: primary is None too.
        let mut none_sel = RoleSelector::new(config.clone(), Arc::new(MapStakeProvider::with_default(0)));
        none_sel.select();
        assert_eq!(none_sel.select_primary(), None);
    }
}

/// Test-only `StakeProvider` implementations and helpers. Kept at module scope
/// (test-gated) so every discriminator shares one in-memory stake map rather
/// than re-reading disk config or touching the data service.
#[cfg(test)]
mod test_helpers {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pneumatic_core::config::Config;
    use pneumatic_core::node::{NodeRegistryType, NodeTypeConfig};
    use strum::IntoEnumIterator;

    use super::StakeProvider;

    /// In-memory epoch->stake map, key-agnostic: the selector reads its *own*
    /// config `public_key`, so the provider must not depend on which key is
    /// asked for. Fail-closed: an unset epoch reports `default` (tests make
    /// that 0 to exercise the zero-stake path).
    #[derive(Default)]
    pub struct MapStakeProvider {
        values: HashMap<u64, u64>,
        default: u64,
        /// Number of times `stake` was consulted — proves `select()` does not
        /// look the stake up once per role type.
        pub lookups: Arc<AtomicUsize>,
    }

    impl MapStakeProvider {
        pub fn with_default(default: u64) -> Self {
            Self { values: HashMap::new(), default, lookups: Arc::new(AtomicUsize::new(0)) }
        }
        pub fn with_epoch(&mut self, epoch: u64, stake: u64) -> &mut Self {
            self.values.insert(epoch, stake);
            self
        }
    }

    impl StakeProvider for MapStakeProvider {
        fn stake(&self, _public_key: &[u8], epoch: u64) -> u64 {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            self.values.get(&epoch).copied().unwrap_or(self.default)
        }
    }

    /// Per-type floor builder: a `Config` whose every type requires `floor`
    /// stake. The environment registry is left empty so the global floor falls
    /// back to `CostModel::default_global_min_stake()` (10) — deterministic.
    pub fn config_with_type_floor(floor: u64) -> Arc<Config> {
        let type_configs = Arc::new(dashmap::DashMap::new());
        for t in NodeRegistryType::iter() {
            type_configs.insert(t, NodeTypeConfig { min: 1, max: 1000, min_stake: floor });
        }
        Arc::new(Config::new_for_testing("env".into(), Arc::new(dashmap::DashMap::new()), type_configs))
    }
}
