//! Core library for the pneumatic blockchain protocol.
//!
//! Pneumatic is a proof-of-stake protocol where **every token is its own
//! independent, parallel blockchain** — a lattice of per-token hash-chained
//! ledgers rather than one global chain (see `tokens.rs`, `blocks.rs`).
//! Consensus is executed by four staked worker roles in a pipeline —
//! sentinel (validate + gossip), executor (deterministic execution),
//! finalizer (stake-weighted finalization), committer (nonce/gas/stake
//! routing and commit) — with transactions moving through an explicit state
//! machine (`transactions.rs`: Pending -> Preloaded -> Validated -> Executing
//! -> Finalizing -> Committed). The design is trait-first: transport
//! (`Connection`, `Stream`, `Sender`, `Listener`), storage (`DataProvider`),
//! validation (`TransactionValidationSpec` / `BlockValidatorSpec`), and
//! logging are traits so worker binaries and tests can substitute
//! implementations. Inter-service traffic on the socket path is MsgPack over
//! 4-byte-big-endian-length-prefixed frames (`conns.rs`, `encoding.rs`);
//! RNS (Reticulum) is the production inter-node transport (`rns/`), carrying
//! hybrid post-quantum signatures (Ed25519 · ML-DSA-44, plus X25519 ·
//! ML-KEM-768 key encapsulation and AES-256-GCM encryption in `crypto.rs`).
//! Tier-1 shielded value transfer is proven with halo2 (no trusted setup) in
//! `shielded/`. This crate is a library: the `sentinel`, `executor`,
//! `finalizer`, and `committer` worker binaries in sibling crates depend on
//! it.
//!
//! ## Module guide
//!
//! * [`config`] — `Config`: loads `config.json` plus per-environment specs,
//!   assembles node configuration, RNS identity, and bootstrap peers.
//! * [`conns`] — connectivity layer: port table, 4-byte length-prefixed frame
//!   readers (`get_data` / `get_data_async`), `Connection` trait, and the
//!   sync/async stream, sender, listener, and factory submodules.
//! * [`contracts`] — contract-execution substrate: pluggable `ContractEngine`
//!   trait, canonical `ExecutionInput`/`ExecutionOutput`, and the name-keyed
//!   engine registry (determinism is consensus-critical; ADR-011/ADR-013).
//! * [`crypto`] — `AsymCryptoProvider` trait with hybrid classical · PQC
//!   implementation (Ed25519 · ML-DSA-44 signing, X25519 · ML-KEM-768,
//!   AES-256-GCM) and the SHA-256 `HashProvider`.
//! * [`data`] — `DataProvider` trait abstracting the external data store;
//!   `DefaultDataProvider` talks MsgPack frames over UDS (TCP loopback on
//!   non-Unix) to the local data service, with an in-process cache.
//! * [`encoding`] — JSON and MsgPack (de)serialization helpers; MsgPack uses
//!   named maps so `skip_serializing_if` fields cannot shift positions.
//! * [`environment`] — `EnvironmentMetadata`: per-environment partitions,
//!   crypto provider, cost model, quorum settings, and validator/engine
//!   registries.
//! * [`errors`] — `PneumaticError`, the workspace-wide error type, with
//!   structured validation-failure reasons and risk factors.
//! * [`gossiper`] — `Gossiper`: fan-out of signed messages to registered
//!   peers with content-hash dedup backed by a TTL cache.
//! * [`logging`] — `Logger` trait and `FileLogger` (file-locked append
//!   writes), the consensus-side durable log channel.
//! * [`messages`] — the wire `Message` struct (chain id, action, MsgPack
//!   body, hybrid signature) plus ack/reject and signing helpers.
//! * [`node`] — node and registry types (Full/Light; Committer, Sentinel,
//!   Executor, Finalizer, Archiver); `node::registry` holds the `NodeRegistry`
//!   (per-type DashMap of connected nodes, registration, heartbeat, fan-out).
//! * [`server`] — `ThreadPool`: hybrid sync + async worker pool with
//!   configurable thread count.
//! * [`tokens`] — `Token`: metadata, its own `Blockchain`, optional asset
//!   data, and per-token block-validation hooks; token factory/mint types.
//! * [`blocks`] — `Block` and `Blockchain`: the per-token hash-chained
//!   append-only chain with optimistic/confirmed finality tracking.
//! * [`transactions`] — transaction models and the explicit
//!   `TransactionState` lifecycle, including leader/finalizer/executor
//!   signature aggregation and `TransactionCommit`.
//! * [`validation`] — `TransactionValidationSpec` / `BlockValidatorSpec`
//!   traits, spec registries, and the self-signed, executed, upgrade, and
//!   shielded validator specs (fail-closed).
//! * [`registry`] — `PendingTransactionRegistry` (concurrent in-flight
//!   transactions), plus nullifier and replay-signature registries.
//! * [`epoch`] — epochs, stake/executor sets, deterministic domain-separated
//!   leader and shard selection, block proposing, and candidate registry.
//! * [`action_router`] — `ActionRouter`: sender-side nonce, gas, and stake
//!   checks and routing of validated transactions to their target token.
//! * [`auth`] — C1 envelope authentication: verify the envelope signature and
//!   resolve the sender's registered roles, fail-closed on either step.
//! * [`user`] — `User`: protocol-level gas balance, global stake, and
//!   per-sender nonce, stored outside any token.
//! * [`rns`] — RNS (Reticulum) transport: identity keystore, the single
//!   choke-point `NodeConfig` builder, the network wrapper (worker pool +
//!   Resource-based large-payload path), and the `Connection` impl.
//! * [`shielded`] — Tier-1 shielded value transfer: Poseidon hash, note
//!   commitment/nullifier derivation, Merkle tree, the halo2 action circuit,
//!   and network-side proof verification.
//! * [`telemetry`] — ops telemetry: `tracing` init, a lock-free metrics
//!   registry rendered in Prometheus text format, a minimal health/metrics
//!   HTTP endpoint, and a shutdown-signal helper.

pub mod config;
pub mod conns;
pub mod contracts;
pub mod crypto;
pub mod data;
pub mod encoding;
pub mod environment;
pub mod errors;
pub mod gossiper;
pub mod logging;
pub mod messages;
pub mod node;
pub mod server;
pub mod tokens;
pub use tokens::{MintArgs, MintResult, TokenFactory};
pub mod blocks;
pub mod transactions;
pub mod validation;
pub mod registry;
pub use registry::{NullifierRegistry, PendingAdminCredit, PendingTransactionRegistry};
pub mod epoch;
pub use epoch::deterministic_select;
pub use epoch::deterministic_select_shard;
pub use epoch::ExecutorSet;
pub mod action_router;
pub mod auth;
pub mod user;
pub mod rns;
pub mod shielded;
pub mod telemetry;
