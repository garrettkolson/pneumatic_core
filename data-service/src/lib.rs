//! `pneumatic_data_service` — the data service the node binaries talk to.
//!
//! # What this crate is
//!
//! `pneumatic_core::data::DefaultDataProvider` is the **client** half of the
//! data channel: every node's chain state, user rows, stake snapshots,
//! executor sets, and shielded-pool state are read and written through it as
//! framed MsgPack requests to a local or remote service. Until now the repo
//! shipped only that client half — the operator runbook records the service as
//! "not part of this repo", and both binaries fail closed at boot without it,
//! which makes any multi-node run (and the Phase 8 compose file) impossible to
//! start. This crate is the service half.
//!
//! ```text
//!   node process                         this crate
//!   ────────────                         ──────────
//!   DefaultDataProvider  -- frame ----►  server::serve_connection
//!        (client)          │                     │
//!                          │  [4B len][HMAC][rmp] │
//!                          ▼                      ▼
//!                   one request / connection   DataStore (byte-opaque KV)
//! ```
//!
//! # The three modules
//!
//! * [`store`] — a `(partition_id, key) → bytes` map with optional write-through
//!   persistence. Byte-opaque on purpose: integrity is the envelopes' job,
//!   verified by the client on load.
//! * [`server`] — the frame codec and accept loop, reusing the client's own
//!   `conns::uds::sign_payload` / `verify_payload` so authentication cannot drift.
//! * [`genesis`] — the records a cluster needs before its first node can boot
//!   (users, stake snapshots, the pristine shielded pool, the partition token),
//!   written **through the client API** so envelope fingerprints come from
//!   `pneumatic_core` and never a second implementation.
//!
//! # Embedding
//!
//! [`server::spawn`] binds an ephemeral port and returns the bound address, so
//! an integration test can boot a real service and point a real
//! `DefaultDataProvider` at it — the seam that proves a node can actually boot
//! against this service rather than only against an in-memory stub.

pub mod genesis;
pub mod server;
pub mod store;

pub use genesis::{
    apply as apply_genesis, genesis_pool_state, load_spec, GenesisAccount, GenesisError,
    GenesisNode, GenesisReport, GenesisSpec,
};
pub use server::{serve, spawn};
pub use store::DataStore;
