//! The boot-gate test: can a real node actually start against the real data
//! service?
//!
//! Both node binaries fail closed at boot on exactly two data reads, and those
//! two reads are the reason a multi-node testnet was impossible to spin up
//! until now:
//!
//! ```text
//! committer/src/main.rs      get_stake_snapshot(1, token_partition)  .expect("load stake snapshot at boot")
//!                            ShieldedPool::load(...)                 .expect("load shielded pool at boot")
//! node-server build.rs       ShieldedPool::load(...)                 (composite boot, same contract)
//! ```
//!
//! So this test drives those exact calls — the committer's own
//! [`ShieldedPool::load`], not a reimplementation — over a real socket to a
//! real service, before and after genesis. The empty-store half is as important
//! as the seeded half: it pins the fail-closed contract the operator runbook
//! documents ("fix the data channel, do not restart-loop") and proves genesis
//! is what converts an unbootable store into a bootable one.

use std::net::SocketAddr;
use std::sync::Arc;

use pneumatic_core::conns::ConnTarget;
use pneumatic_core::data::{DataProvider, DefaultDataProvider};

use pneumatic_committer::shielded_pool::ShieldedPool;
use pneumatic_data_service::{apply_genesis, spawn, DataStore, GenesisNode, GenesisSpec};

const PARTITION: &str = "token";
/// The recency window the deploy env spec carries; a mismatch would make a
/// booting node rebuild the pool with a different window.
const RECENCY: usize = 10;

/// Boot a service on an ephemeral port and hand back the production client
/// pointed at it (the same construction as `bin/node-server.rs`).
fn boot() -> (Arc<DataStore>, Arc<dyn DataProvider>, SocketAddr) {
    let store = Arc::new(DataStore::new());
    let (bound, handle) = spawn(
        "127.0.0.1:0".parse::<SocketAddr>().expect("loopback:0"),
        store.clone(),
        None,
    )
    .expect("the data service binds an ephemeral port");
    std::mem::forget(handle); // keep the accept loop alive for the test's scope

    let provider: Arc<dyn DataProvider> = Arc::new(
        DefaultDataProvider::new().with_source(ConnTarget::Remote(bound)),
    );
    (store, provider, bound)
}

fn genesis_spec(node_keys: &[Vec<u8>]) -> GenesisSpec {
    GenesisSpec {
        environment_id: "env".to_string(),
        token_partition_id: PARTITION.to_string(),
        stake_snapshot_epochs: vec![0, 1],
        shielded_root_recency: RECENCY,
        nodes: node_keys
            .iter()
            .map(|key| GenesisNode {
                public_key_hex: hex::encode(key),
                stake: 1_000,
                fuel_balance: 100_000,
            })
            .collect(),
        accounts: Vec::new(),
        seed_shielded_pool: true,
        seed_partition_token: true,
    }
}

#[test]
fn an_ungenicented_store_is_refused_at_boot_by_both_binaries_contract() {
    let (_store, provider, _addr) = boot();

    // The committer's exact call, and its exact failure mode.
    let snapshot = provider.get_stake_snapshot(1, PARTITION);
    assert!(
        snapshot.is_err(),
        "an empty store must not answer the boot stake-snapshot read"
    );

    // The composite/committer pool load is fail-closed on Err — it must not
    // treat "cannot read" as "no state", because re-seeding would forget prior
    // spends.
    let err = ShieldedPool::load(&*provider, PARTITION, RECENCY)
        .expect_err("a missing pool is indistinguishable from a corrupt one, by design");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("refusing to re-seed"),
        "the boot error must name the fail-closed reason, got: {rendered}"
    );
}

#[test]
fn genesis_makes_the_committer_boot_path_succeed() {
    // Three validators, as a minimal real cluster.
    let keys: Vec<Vec<u8>> = (1u8..=3).map(|i| vec![i; 32]).collect();
    let (_store, provider, _addr) = boot();

    apply_genesis(&genesis_spec(&keys), &*provider).expect("genesis applies against a live service");

    // --- committer boot step 1: the stake snapshot ------------------------
    let snapshot = provider
        .get_stake_snapshot(1, PARTITION)
        .expect("load stake snapshot at boot")
        ;
    assert_eq!(snapshot.stakers.len(), 3, "every validator must be in the boot snapshot");
    for key in &keys {
        assert!(
            snapshot.stakers.contains_key(key),
            "a validator missing from the snapshot would have its registrations rejected"
        );
    }

    // --- committer boot step 2: the shielded pool -------------------------
    // This is the strongest assertion in the file: `load` rebuilds the tree
    // from the applied deltas and cross-checks the recorded root, so an `Ok`
    // here means the genesis pool state is byte-consistent with what the
    // committer itself would persist.
    let pool = ShieldedPool::load(&*provider, PARTITION, RECENCY)
        .expect("load shielded pool at boot");
    assert_eq!(pool.leaf_count(), 0, "a genesis pool has no notes");

    // Epoch 0 too — the standard pipeline path reads it for quorum checks.
    provider
        .get_stake_snapshot(0, PARTITION)
        .expect("epoch 0 snapshot must also exist for the pipeline path");

    // --- role selection: the composite reads its own stake from a user row --
    for key in &keys {
        let user = provider
            .get_user(key, PARTITION)
            .expect("role selection reads User.stake");
        assert!(user.stake > 0, "a node with zero stake installs no roles");
    }
}

#[test]
fn a_second_boot_against_the_same_store_loads_the_persisted_pool() {
    // The restart case: genesis, boot, boot again. The second load must read
    // the persisted record rather than re-deriving a pristine pool — the exact
    // drift the fail-closed contract exists to prevent.
    let keys = vec![vec![0x7du8; 32]];
    let (_store, provider, _addr) = boot();
    apply_genesis(&genesis_spec(&keys), &*provider).expect("genesis");

    let first = ShieldedPool::load(&*provider, PARTITION, RECENCY).expect("boot #1");
    let second = ShieldedPool::load(&*provider, PARTITION, RECENCY).expect("boot #2");

    assert_eq!(first.leaf_count(), second.leaf_count());
    assert_eq!(
        first.current_root(),
        second.current_root(),
        "boot #2 must load the stored root, not a fresh genesis"
    );
}
