---
id: fact-data-service
title: "Data service ships in-repo (data-service crate)"
type: fact
namespace: pneumatic
visibility: namespace
summary: "The required data service now ships in-repo as the data-service crate / pneumatic_data_service binary: framed MsgPack store speaking DataOp, byte-opaque by design, plus a genesis seeder that writes through the client API so envelope fingerprints cannot drift."
auto_inject: false
applicable_when: "Running any node binary, spinning up a testnet, or touching the data channel"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "If data-service/ is removed, or if DataOp/GetOp/SaveOp gain a variant the dispatch does not handle, or if the frame format in conns/senders.rs changes"
tags: [data-service, data-provider, genesis, boot, testnet, deployment]
edges:
  - target: concept-data-provider
    type: depends_on
    weight: 0.95
    note: "The crate is the server half of DefaultDataProvider's channel; it reuses the client's own framing/auth helpers"
  - target: fact-workspace-layout
    type: related_to
    weight: 0.8
    note: "The 8th crate and the 3rd shipped binary"
  - target: concept-shielded-pool
    type: supports
    weight: 0.7
    note: "The pool's fail-closed boot load is why genesis must seed a pool record; an empty store is unbootable"
  - target: concept-sentinel-role
    type: related_to
    weight: 0.5
    note: "The sentinel calls latest_block_hash(environment_id), which resolves as get_token(env_id, env_id) — so genesis must seed a partition token or routing errors on the first transaction"
  - target: task-testnet-launcher
    type: followed_by
    weight: 0.85
    note: "The service plus genesis unblocks a cluster; the launcher/peering work is the remaining path to a multi-node testnet"
related: ["[[Workspace layout: 8 crates, 25 root modules, 3 binaries]]", "[[DataProvider: MsgPack data-store trait with UDS-first local transport and HMAC option]]"]
source_url: "Empty"
---

# Data service ships in-repo (data-service crate)

Code- and test-verified 10/02/2026.

Until 10/02/2026 the repo shipped only the **client** half of the data channel
(`DefaultDataProvider`) — the operator runbook recorded the service itself as
"not part of this repo", and because both node binaries fail closed at boot
without it, **no multi-node run and not even the Phase 8 compose file could
actually start**. `data-service/` (crate `pneumatic_data_service`, binary
`pneumatic_data_service`) is the server half.

**Wire contract** (`data-service/src/server.rs`): `[4B BE length][32B HMAC tag][rmp body]`,
one request per connection (the client's `Sender::get_response` has no
keep-alive). The body is `DataRequest{key, op, partition_id}`. Frame tag and
verification are the client's own `conns::uds::sign_payload` /
`verify_payload`, so authentication cannot drift between the halves. Required a
new additive seam: `DataRequest::key() / op() / partition_id()` accessors
(`src/data.rs`) — fields stay private because the wire shape is consensus
surface, but a service cannot dispatch a request it cannot read.

**The store is byte-opaque on purpose** (`data-service/src/store.rs`): integrity
lives in the SHA-256 envelopes that the **client** re-verifies on load
(`get_stake_snapshot`, `get_shielded_pool`), so the store keeps exactly the bytes
a `Save` carried and returns them verbatim. Recomputing hashes server-side would
be a second implementation of a consensus-critical fingerprint. Absence is not
modelled as a value: the protocol has no not-found signal, so a miss returns an
empty body and the client surfaces `DeserializationError` — the conservative
direction, since inventing a value would be trusted.

**Genesis is written through the client API** (`data-service/src/genesis.rs`):
`apply(spec, &dyn DataProvider)` calls `save_user` / `save_stake_snapshot` /
`save_shielded_pool` / `save_token`, so envelope fingerprints come from
`pneumatic_core`'s own save path. Four records are load-bearing, and each is read
by a different component — user rows (role selection), epoch-1 stake snapshot
(registration gate), epoch-0 snapshot (pipeline-path quorum), shielded pool
(composite boot, fail-closed) — plus a token keyed by `environment_id` for the
sentinel's `latest_block_hash`. The pristine pool root is *derived*
(`root_to_bytes(empty tree.root())`), which equals the `MerkleRootState::new`
genesis tip, so a booting committer's rebuild-and-compare integrity check passes.

**Proof, not assertion** — `tests/data_service_boot.rs` drives the committer's
own `ShieldedPool::load` over a real socket: an un-genesis-ed store fails with
"refusing to re-seed" (the runbook contract, pinned) and a genesis-seeded one
boots. `data-service/tests/service_roundtrip.rs` (14 tests) drives a real
`DefaultDataProvider` end to end, including HMAC rejection and state-file restart.

**Scale note for a testnet:** the store's write-through persistence serializes
the whole map per write (fine for a testnet, a redesign item for production), and
the service keeps no per-node state — but the genesis stake set is the single
source of truth for who may join, so a launcher must know every node's Ed25519
key before first boot.
