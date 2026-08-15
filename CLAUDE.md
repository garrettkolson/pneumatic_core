# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a Rust workspace for the "pneumatic" blockchain protocol: the root `pneumatic_core` crate is a library (no binary target) with the core protocol modules, and `sentinel`, `executor`, `finalizer`, and `committer` are the four worker-node crates that depend on it.

## Build & Test Commands

```bash
cargo check              # Quick compilation check
cargo build              # Build the library
cargo test               # Run all 398 tests (5-crate workspace)
cargo test <filter>      # Run a single test, e.g. cargo test blocks::tests::get_current_chain_state_with_empty_chain
```

## Architecture

### Top-level modules (lib.rs)

The library is flat — 20 modules declared in `src/lib.rs`:

| Module | Responsibility |
|--------|---------------|
| `node` | Node types (Full/Light), registry types (Committer, Sentinel, Executor, Finalizer, Archiver), registration request/response protocol |
| `node::registry` | `NodeRegistry` — manages per-type DashMap collections of connected nodes, accepts connections, broadcasts messages |
| `conns` | Network connectivity: traits, port constants, sync/async data reading helpers, `Connection` trait, `TcpConnection` |
| `conns::factories` | `ConnFactory` — creates Senders, Listeners, and Connections for TCP and Unix domain sockets |
| `conns::senders` | `Sender` trait with `TcpSender` and `UdsSender` implementations (blocking send + receive over TCP/UDS) |
| `conns::streams` | `Stream` trait (sync read/write), `StreamReader`/`StreamWriter` traits (async via Tokio), wrapping `TcpStream` and `UnixStream` |
| `conns::listeners` | `Listener` trait with `CoreTcpListener` and `CoreUdsListener` |
| `server` | `ThreadPool` — hybrid sync+async worker pool with configurable thread count |
| `config` | `Config` — loads `config.json` and per-environment specs from `/env/`, builds node configuration |
| `environment` | `EnvironmentMetadata` — environment-level config with partition definitions, crypto provider, quorum settings, block validators, logger |
| `data` | `DataProvider` trait — abstracts external data store; `DefaultDataProvider` communicates via TCP/UDS to a local data service using MsgPack |
| `crypto` | `AsymCryptoProvider` trait — `Ed25519Provider` (sign/verify/public_key via ed25519-dalek, encrypt/decrypt for self-encryption, encrypt_to/decrypt_from for cross-recipient encryption via AES-256-GCM + X25519 DH; `x25519_public_key()` accessor), `HashProvider` trait with `BasicHashProvider` (SHA-256 via ring) |
| `encoding` | JSON and MsgPack (rmp-serde) serialization/deserialization helpers |
| `errors` | `PneumaticError` — workspace-wide error type |
| `gossiper` | `Gossiper` — fan-out of messages to connected nodes with dedup |
| `validation` | `TransactionValidationSpec` / `BlockValidatorSpec` traits, spec registries, nonce validation |
| `registry` | `PendingTransactionRegistry` — DashMap of in-flight transactions |
| `epoch` | `Epoch`, `StakeSet`, `ExecutorSet`, `LeaderSelector`, `BlockProposer`, `CandidateRegistry`, `IStakingManager` / `IEpochReconciler` (with stubs) |
| `action_router` | `ActionRouter` — nonce/gas/stake checks and per-transaction routing |
| `tokens` | `Token` — contains metadata, blockchain, optional asset data; `BlockValidator` trait for per-token block validation |
| `blocks` | `Block` and `Blockchain` — append-only chain with hash chaining; `BlockFactory` for hash computation |
| `transactions` | `Transaction`, `SignedTransaction`, `TransactionCommit` — models the signed transaction structure with leader/finalizer/executor signatures |
| `messages` | Wire message format (`Message` struct), ack/reject helpers |
| `logging` | `Logger` trait with `FileLogger` implementation (file-locked append writes) |
| `user` | Minimal `User` struct with `fuel_balance` |

### Key design patterns

- **Trait-based abstraction**: `Connection`, `Sender`, `Stream`, `Listener`, `DataProvider`, `BlockValidator`, `Logger`, `AsymCryptoProvider` — all traits with concrete implementations
- **DashMap for concurrent registry**: Node connections are stored in `DashMap<Vec<u8>, NodeRegistryNode>` keyed by node public key
- **MsgPack wire format**: Inter-service communication uses rmp-serde serialization over length-prefixed TCP/UDS frames
- **Port-per-type**: Each node registry type has dedicated external and internal port numbers (e.g., Committer=42001 external, 50000 internal)
- **Environment-driven config**: Node behavior is parameterized by JSON config files + per-environment JSON specs

### Wire protocol

Data frames consist of a 4-byte big-endian length header followed by the MsgPack-serialized payload. Read in two steps: read 4 bytes for length, then read that many bytes.

## Important notes

- Former `todo!()` stubs are resolved: `BlockFactory::create_hash` (blocks.rs:85) computes SHA-256 over `previous_hash || timestamp || signed_trans || token_metadata`; `Token::get_asset_mut` was removed; `MessageBody<T>` (messages.rs:28) is `{action, body}`
- The ThreadPool's sync worker loops properly (server.rs:118), and the zero-thread panic test is an active, passing `#[should_panic]` (server.rs:169-173)
- `data.rs` uses Unix domain sockets on Unix platforms, falling back to TCP loopback (port 55555) on non-Unix
- `NodeRegistry::send_to_all` (node/registry.rs:167) sends over registered connections; one TODO remains at node/registry.rs:74 (placeholder public node address)
- Staking persistence is still stubbed — `StubStakingManager` (epoch.rs:363) logs `StakingOp`s without persisting them
