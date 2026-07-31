# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**pneumatic_core** is a Rust library (edition 2021) — the core module for worker node processes operating the "pneumatic" blockchain protocol. There is no binary target.

## Build & Test Commands

```bash
cargo check              # Quick compilation check
cargo build              # Build the library
cargo test               # Run all 28 tests
cargo test <filter>      # Run a single test, e.g. cargo test blocks::tests::get_current_chain_state_with_empty_chain
```

## Architecture

### Top-level modules (lib.rs)

The library is flat — 13 modules declared in `src/lib.rs`:

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
| `crypto` | `AsymCryptoProvider` trait (RSA placeholder) and `HashProvider` trait — both currently `todo!()` stubs |
| `encoding` | JSON and MsgPack (rmp-serde) serialization/deserialization helpers |
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

- Several modules contain `todo!()` stubs: `crypto.rs` (RSA encrypt/decrypt/sign/check_signature), `tokens.rs` (`get_asset_mut`), `blocks.rs` (`BlockFactory::create_hash` should actually hash), `messages.rs` (`MessageBody` struct empty)
- The `server.rs` ThreadPool has a commented-out async poison test (`#[should_panic]` test at line 252-275) that hangs and needs fixing
- The ThreadPool's sync worker processes one job then exits (not a proper loop) — see `Worker::get_sync_thread` line 118
- `config.rs` line 37-47 has `// todo` comments for node registry type selection, connection count calculation, and minimum stake
- `data.rs` uses Unix domain sockets on Unix platforms, falling back to TCP loopback (port 55555) on non-Unix
- `node::registry.rs` line 165 has a TODO to use registered connections instead of creating senders on the fly for `send_to_all`
