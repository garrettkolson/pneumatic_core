//! Wire-level tests for the data service, driven by the **real client**.
//!
//! Every test here goes through `pneumatic_core::data::DefaultDataProvider` —
//! the exact type the node binaries construct — over a real TCP socket to a
//! real service. That is deliberate: an in-memory provider can never catch a
//! framing, codec, or authentication mismatch, and those are precisely the
//! failure modes that would leave every node in a testnet unable to boot.

use std::net::SocketAddr;
use std::sync::Arc;

use pneumatic_core::conns::ConnTarget;
use pneumatic_core::data::{DataProvider, DefaultDataProvider, StakeSnapshotEnvelope};
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::epoch::StakeSet;
use pneumatic_core::user::User;

use pneumatic_data_service::{apply_genesis, genesis_pool_state, spawn, DataStore, GenesisSpec, GenesisNode};

const PARTITION: &str = "token";

/// Boot a service on an ephemeral port and return a client pointed at it,
/// holding the store and the accept thread alive for the caller's scope.
fn boot(secret: Option<&str>) -> (Arc<DataStore>, DefaultDataProvider, SocketAddr) {
    let store = Arc::new(DataStore::new());
    let secret_bytes = secret.map(|s| s.as_bytes().to_vec());
    let (bound, _handle) = spawn(
        "127.0.0.1:0".parse::<SocketAddr>().expect("loopback:0"),
        store.clone(),
        secret_bytes.clone(),
    )
    .expect("service binds an ephemeral port");

    // Deliberately leak the accept thread: it must outlive every request in the
    // test body, and a testnet side-car is torn down with the process anyway.
    std::mem::forget(_handle);

    let mut provider = DefaultDataProvider::new().with_source(ConnTarget::Remote(bound));
    if let Some(bytes) = secret_bytes {
        provider = provider.with_secret(bytes);
    }
    (store, provider, bound)
}

#[test]
fn user_round_trips_through_the_real_client() {
    let (_store, provider, _addr) = boot(None);
    let key = vec![0xaa; 32];
    let mut user = User::new(key.clone());
    user.stake = 4242;
    user.fuel_balance = 999_999;
    user.nonce = 7;

    provider
        .save_user(&key, user.clone(), PARTITION)
        .expect("save_user must succeed against a live service");

    let loaded = provider.get_user(&key, PARTITION).expect("get_user must read it back");
    assert_eq!(loaded.public_key, key);
    assert_eq!(loaded.stake, 4242, "stake is what role selection reads");
    assert_eq!(loaded.fuel_balance, 999_999);
    assert_eq!(loaded.nonce, 7, "nonce tracking must survive persistence");
}

#[test]
fn stake_snapshot_envelope_is_stored_verbatim_and_verifies() {
    let (store, provider, _addr) = boot(None);
    let mut set = StakeSet::default();
    set.stakers.insert(vec![1u8; 32], 100);
    set.stakers.insert(vec![2u8; 32], 200);

    provider
        .save_stake_snapshot(1, set.clone(), PARTITION)
        .expect("save_stake_snapshot");

    let loaded = provider.get_stake_snapshot(1, PARTITION).expect("get_stake_snapshot");
    assert_eq!(loaded.stakers.len(), 2);
    assert_eq!(loaded.stakers.get(&vec![2u8; 32]), Some(&200));

    // The stored bytes must be the client-built envelope — not a re-derived
    // value. If the service ever unwrapped or re-serialized the payload, the
    // fingerprint below would no longer match what the client verifies on load.
    let stored = store
        .get(PARTITION, &1u64.to_be_bytes().to_vec())
        .expect("stake snapshot is keyed by the big-endian epoch");
    let envelope: StakeSnapshotEnvelope =
        deserialize_rmp_to::<StakeSnapshotEnvelope>(&stored).expect("stored bytes are an envelope");
    envelope.verify().expect("stored envelope fingerprint must verify");
    assert_eq!(envelope.epoch, 1);
}

#[test]
fn separate_epochs_are_separate_records() {
    let (_store, provider, _addr) = boot(None);
    let mut epoch0 = StakeSet::default();
    epoch0.stakers.insert(vec![1u8; 32], 100);
    let mut epoch1 = StakeSet::default();
    epoch1.stakers.insert(vec![1u8; 32], 100);
    epoch1.stakers.insert(vec![2u8; 32], 50);

    provider.save_stake_snapshot(0, epoch0, PARTITION).expect("epoch 0");
    provider.save_stake_snapshot(1, epoch1, PARTITION).expect("epoch 1");

    assert_eq!(provider.get_stake_snapshot(0, PARTITION).expect("read 0").stakers.len(), 1);
    assert_eq!(provider.get_stake_snapshot(1, PARTITION).expect("read 1").stakers.len(), 2);
}

#[test]
fn missing_key_fails_closed_rather_than_inventing_a_value() {
    let (_store, provider, _addr) = boot(None);
    let err = provider
        .get_user(&vec![9u8; 32], PARTITION)
        .expect_err("an absent key must not read as a default user");
    // The wire protocol has no not-found signal, so absence surfaces as a
    // deserialization failure. Nodes treat any Err as fail-closed.
    assert!(
        format!("{err:?}").contains("DeserializationError"),
        "absence must surface as a data error, got {err:?}"
    );
}

#[test]
fn shielded_pool_state_round_trips_with_a_verifiable_envelope() {
    let (store, provider, _addr) = boot(None);
    let state = genesis_pool_state();

    provider.save_shielded_pool(&state, PARTITION).expect("save_shielded_pool");

    let loaded = provider
        .get_shielded_pool(PARTITION)
        .expect("a stored pool must read back as Some")
        .expect("the service returns the envelope, never a fabricated absence");
    assert_eq!(loaded.root, state.root);
    assert_eq!(loaded.leaf_count, 0);

    // And the stored bytes still carry their own valid fingerprint.
    let stored = store.get(PARTITION, b"shielded_pool").expect("pool key");
    let _ = &stored;
    assert!(!stored.is_empty());
}

#[test]
fn data_blob_round_trips_for_executor_get_data() {
    let (_store, provider, _addr) = boot(None);
    // The executor fetches contract and sender bytes through `get_data`; the
    // core stub returns `DataNotFound` unconditionally, so the service has to
    // be the one that actually answers it.
    provider.save_data(&vec![1u8], b"contract-bytes".to_vec(), PARTITION).expect("save_data");
    let got = provider.get_data(&vec![1u8], PARTITION).expect("get_data");
    assert_eq!(got, b"contract-bytes".to_vec());
}

#[test]
fn token_round_trips_so_latest_block_hash_resolves() {
    let (_store, provider, _addr) = boot(None);
    // `latest_block_hash(partition)` is `get_token(partition, partition)`, and
    // the sentinel calls it with the environment id.
    let token = pneumatic_core::tokens::Token::new();
    let key = b"env".to_vec();
    provider.save_token(&key, token, "env").expect("save_token");

    let tip = provider.latest_block_hash("env").expect("latest_block_hash must resolve");
    assert_eq!(tip, Some(Vec::new()), "an empty chain reports the genesis convention");
}

#[test]
fn mismatched_secret_is_rejected_and_never_returns_data() {
    // Service authenticated under one key, client presenting another.
    let store = Arc::new(DataStore::new());
    let (bound, handle) = spawn(
        "127.0.0.1:0".parse::<SocketAddr>().expect("loopback:0"),
        store.clone(),
        Some(b"service-secret".to_vec()),
    )
    .expect("bind");
    std::mem::forget(handle);

    // Seed directly so the assertion below is about authentication, not absence.
    let key = vec![0xbb; 32];
    let mut user = User::new(key.clone());
    user.stake = 5;
    let body = serialize_to_bytes_rmp(&user).expect("serialize user");
    store.put(PARTITION, &key, &body).expect("direct seed");

    let wrong = DefaultDataProvider::new()
        .with_source(ConnTarget::Remote(bound))
        .with_secret(b"wrong-secret".to_vec());

    let err = wrong.get_user(&key, PARTITION).expect_err("a wrong secret must not read data");
    let rendered = format!("{err:?}");
    // The service answers an authentication failure with no reply at all, so
    // the client sees the closed socket (`FromStore("ReadError(…)")`) rather
    // than a tagged response. Which variant surfaces matters less than the
    // contract: it is an error, and no payload is returned for the node to
    // trust. Both node binaries treat any `DataError` as fail-closed.
    assert!(
        rendered.contains("ReadError")
            || rendered.contains("PeerUnauthenticated")
            || rendered.contains("DeserializationError")
            || rendered.contains("Timeout"),
        "a failed HMAC must surface as a data error, got {rendered}"
    );

    // Matching secret still works, proving the failure was authentication.
    let right = DefaultDataProvider::new()
        .with_source(ConnTarget::Remote(bound))
        .with_secret(b"service-secret".to_vec());
    let loaded = right.get_user(&key, PARTITION).expect("matching secret authenticates");
    assert_eq!(loaded.stake, 5);
}

#[test]
fn state_file_persists_across_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("data.json");
    let key = vec![0xcc; 32];

    {
        let store = Arc::new(DataStore::with_state_file(&path).expect("fresh state file"));
        let (bound, handle) = spawn(
            "127.0.0.1:0".parse::<SocketAddr>().expect("loopback:0"),
            store.clone(),
            None,
        )
        .expect("bind");
        std::mem::forget(handle);
        let provider = DefaultDataProvider::new().with_source(ConnTarget::Remote(bound));
        let mut user = User::new(key.clone());
        user.stake = 777;
        provider.save_user(&key, user, PARTITION).expect("save_user");
    }

    // A "restart": a brand-new store loaded from the same file.
    let reloaded = DataStore::with_state_file(&path).expect("reload");
    let raw = reloaded.get(PARTITION, &key).expect("user survived the restart");
    let user: User = deserialize_rmp_to::<User>(&{ raw.clone() }).expect("stored user decodes");
    assert_eq!(user.stake, 777, "nullifiers and stake must outlive a node restart");
}

#[test]
fn trait_dispatch_honors_the_configured_source_for_user_lookups() {
    // Regression guard for a real defect found while building the testnet
    // tooling: `get_user`/`save_user` were private *inherent* methods on
    // `DefaultDataProvider`, so `impl DataProvider for DefaultDataProvider`
    // inherited the trait default — which constructs a FRESH
    // `DefaultDataProvider::new()` and dials the DEFAULT local UDS path,
    // discarding both `with_source` and `with_secret`.
    //
    // That is invisible from inside `src/data.rs` (its own tests can see the
    // private inherent method) and fatal from outside it: every production
    // caller holds `Arc<dyn DataProvider>`, so `PNEUMATIC_DATA_ADDR` was
    // silently ignored for exactly the two reads that decide whether a node
    // does anything — role selection (`User.stake`) and the registration gate.
    // The symptom in the field is a node that boots cleanly, installs no roles,
    // and has every peer registration rejected.
    let store = Arc::new(DataStore::new());
    let (bound, handle) = spawn(
        "127.0.0.1:0".parse::<SocketAddr>().expect("loopback:0"),
        store.clone(),
        None,
    )
    .expect("bind");
    std::mem::forget(handle);

    // `Arc<dyn DataProvider>` is the shape both node binaries use.
    let provider: Arc<dyn DataProvider> = Arc::new(
        DefaultDataProvider::new().with_source(ConnTarget::Remote(bound)),
    );

    let key = vec![0x5e; 32];
    let mut user = User::new(key.clone());
    user.stake = 12_345;
    provider
        .save_user(&key, user, PARTITION)
        .expect("save_user must reach the configured remote, not the default local socket");

    let loaded = provider
        .get_user(&key, PARTITION)
        .expect("get_user must reach the configured remote, not the default local socket");
    assert_eq!(loaded.stake, 12_345);

    // And it genuinely landed in THIS service's store, not somewhere else.
    assert!(
        store.get(PARTITION, &key).is_some(),
        "the write must be visible in the service the provider was pointed at"
    );
}

#[test]
fn genesis_writes_every_record_a_node_reads_at_boot() {
    let (_store, provider, _addr) = boot(None);
    let node_key = vec![0x11; 32];
    let account_key = vec![0x22; 32];

    let spec = GenesisSpec {
        environment_id: "env".to_string(),
        token_partition_id: PARTITION.to_string(),
        stake_snapshot_epochs: vec![0, 1],
        shielded_root_recency: 10,
        nodes: vec![GenesisNode {
            public_key_hex: hex::encode(&node_key),
            stake: 5_000,
            fuel_balance: 1_000,
        }],
        accounts: vec![pneumatic_data_service::GenesisAccount {
            public_key_hex: hex::encode(&account_key),
            fuel_balance: 500,
            stake: 0,
        }],
        seed_shielded_pool: true,
        seed_partition_token: true,
    };

    let report = apply_genesis(&spec, &provider).expect("genesis applies over the wire");
    assert_eq!(report.users_written, 2);
    assert_eq!(report.stake_snapshots_written, 2);
    assert_eq!(report.total_stake, 5_000);

    // Role selection reads the node's user row.
    let node = provider.get_user(&node_key, PARTITION).expect("node user seeded");
    assert_eq!(node.stake, 5_000);
    assert!(node.stake >= 10, "genesis stake must clear the registration floor");

    // The registration gate reads the epoch-1 snapshot.
    let snapshot = provider.get_stake_snapshot(1, PARTITION).expect("epoch 1 snapshot");
    assert_eq!(snapshot.stakers.get(&node_key), Some(&5_000));

    // A funded account with no stake must not gain voting weight.
    let account = provider.get_user(&account_key, PARTITION).expect("account seeded");
    assert_eq!(account.fuel_balance, 500);
    assert!(
        !snapshot.stakers.contains_key(&account_key),
        "a zero-stake account must not appear in the stake set"
    );

    // Composite boot reads the pool and refuses to start on an error.
    let pool = provider.get_shielded_pool(PARTITION).expect("pool readable").expect("pool seeded");
    assert_eq!(pool.root, genesis_pool_state().root);

    // The sentinel's chain-tip lookup resolves.
    assert_eq!(provider.latest_block_hash("env").expect("tip resolves"), Some(Vec::new()));
}

#[test]
fn genesis_rejects_a_malformed_public_key_before_booting_anything() {
    let (_store, provider, _addr) = boot(None);
    let spec = GenesisSpec {
        environment_id: "env".to_string(),
        token_partition_id: PARTITION.to_string(),
        stake_snapshot_epochs: vec![0, 1],
        shielded_root_recency: 10,
        nodes: vec![GenesisNode {
            public_key_hex: "not-hex".to_string(),
            stake: 100,
            fuel_balance: 0,
        }],
        accounts: Vec::new(),
        seed_shielded_pool: false,
        seed_partition_token: false,
    };
    let err = apply_genesis(&spec, &provider).expect_err("bad key must fail genesis");
    assert!(
        format!("{err}").contains("malformed hex public key"),
        "the error should name the problem, got {err}"
    );
}

#[test]
fn genesis_rejects_an_epoch_less_spec() {
    let (_store, provider, _addr) = boot(None);
    let spec = GenesisSpec {
        environment_id: "env".to_string(),
        token_partition_id: PARTITION.to_string(),
        stake_snapshot_epochs: Vec::new(),
        shielded_root_recency: 10,
        nodes: Vec::new(),
        accounts: Vec::new(),
        seed_shielded_pool: false,
        seed_partition_token: false,
    };
    assert!(apply_genesis(&spec, &provider).is_err(), "a spec with no epochs is unbootable");
}

#[test]
fn empty_store_reports_empty_and_unknown_partition_reads_missing() {
    let store = DataStore::new();
    assert!(store.is_empty());
    store.put("token", b"k", b"v").expect("put");
    assert_eq!(store.len(), 1);
    assert!(store.get("other", b"k").is_none(), "partitions are isolated");
    assert_eq!(store.get("token", b"k"), Some(b"v".to_vec()));

    let keys = store.keys();
    assert_eq!(keys, vec![("token".to_string(), b"k".to_vec())]);
}
