---
id: concept-data-provider
title: "DataProvider: MsgPack data-store trait with UDS-first local transport and HMAC option"
type: concept
namespace: pneumatic
visibility: namespace
summary: "DataProvider trait with Default-Provider fallbacks; DefaultDataProvider talks MsgPack over UDS (per-UID) or TCP loopback :55555, with optional HMAC shared secret."
auto_inject: false
applicable_when: "Touching src/data.rs, the local data service, token/user/snapshot persistence, or snapshot-corruption handling"
confidence: 0.95
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If DATA_TCP_PORT/DATA_UNIX_PATH, default_source(), or the StakeSnapshotEnvelope verify flow in src/data.rs changes"
tags: [data, msgpack, uds, provider, snapshot, hmac]
edges:
  - target: concept-conn-abstraction
    type: related_to
    weight: 0.9
    note: "DefaultDataProvider is a direct consumer of ConnFactory/Sender over ConnTarget"
  - target: pattern-verify-before-dedup
    type: supports
    weight: 0.8
    note: "StakeSnapshotEnvelope.verify() before trusting deserialized bytes is the same discipline"
  - target: pillar-block-lattice
    type: related_to
    weight: 0.7
    note: "Stake/executor snapshots and latest_block_hash feed epoch selection and deterministic seeds"
related: []
source_url: "Empty"
---

# DataProvider: MsgPack data-store abstraction

`DataProvider` (src/data.rs:20-68) abstracts the external data service. The trait's first seven methods (`get_token`/`save_token`/`get_data`/`save_data`/`get_user`/`save_user`) have **default implementations that forward to `DefaultDataProvider`**, so custom providers only override what they serve; `get_stake_snapshot`/`save_stake_snapshot`/`get_executor_set`/`save_executor_set` are required, and `latest_block_hash` defaults to `Ok(None)` so providers that don't track the chain tip need no change (data.rs:65-67).

`DefaultDataProvider` (data.rs:70-74) is a thin RPC client: `{conn_factory: ConnFactory, source: ConnTarget}`. The default endpoint (`default_source()`, data.rs:79-90) is **per-UID Unix domain socket** `data` (via `data_socket_path`, i.e. `$XDG_RUNTIME_DIR/pneumatic/data.sock`) on Unix, falling back to **TCP loopback port 55555** (`DATA_TCP_PORT`, data.rs:17) elsewhere. The old relative, world-writable `"data"` path was removed because a pre-created symlink could hijack it (data.rs:76-78).

Each call serializes a `DataRequest` (key + `DataOp` + partition) to MsgPack, sends it through `conn_factory.get_sender(source)` + `sender.get_response` (data.rs:148-168), and deserializes the response. `with_secret` (data.rs:114-117) wires a shared secret into the factory so every frame is HMAC-authenticated. `conn_error_to_data_error` (data.rs:95-101) maps blocked I/O to `DataError::Timeout` (a hung data service must not block forever) and auth failures to `PeerUnauthenticated`.

Snapshot reads are corruption-checked: `get_stake_snapshot` (data.rs:237-250) deserializes a `StakeSnapshotEnvelope`, calls `env.verify()` (SHA-256 fingerprint), and surfaces mismatches as `SnapshotCorrupt` instead of silently trusting bytes.
