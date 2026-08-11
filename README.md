# pneumatic_core

**Pneumatic** is a Rust implementation of a proof-of-stake blockchain protocol for distributed worker node networks. It provides the full transaction pipeline — from submission through validation, execution, finalization, and commitment — with support for self-signed tokens, stake-weighted leader election, epoch-based consensus, and hybrid AES-256-GCM encryption.

[![Rust](https://img.shields.io/badge/Rust-2021-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

---

## Quick Start

```bash
cargo check           # Verify compilation
cargo build           # Build all workspace crates
cargo test --workspace --lib   # Run 303 tests across 5 crate targets
cargo test <filter>   # Run a single test, e.g. cargo test leader_selector
```

## Workspace Structure

```
pneumatic_core/
├── src/                        # pneumatic_core crate — core library (21 modules)
├── sentinel/                   # pneumatic_sentinel crate — transaction validation & routing
├── executor/                   # pneumatic_executor crate — contract execution
├── finalizer/                  # pneumatic_finalizer crate — quorum & block building
├── committer/                  # pneumatic_committer crate — chain commitment & epochs
├── Cargo.toml                  # Workspace root + core crate config
├── Cargo.lock
├── TASKS.md                    # Detailed implementation checklist
└── CLAUDE.md                   # Development guidance
```

### Cargo Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1.44.2 | Async runtime |
| `dashmap` | 7.0.0-rc0 | Concurrent HashMap for node/transaction registries |
| `moka` | 0.12.10 | TTL-backed message dedup cache |
| `ed25519-dalek` | 2.0 | Ed25519 signatures (sign/verify) |
| `ring` | 0.17.14 | SHA-256 hashing |
| `aes-gcm` | 0.11.0 | AES-256-GCM encryption |
| `x25519-dalek` | 3.0.0 | X25519 Diffie-Hellman key exchange |
| `serde` / `serde_json` / `rmp-serde` | 1.0 | JSON + MsgPack serialization |
| `rand` | 0.8 | Deterministic stake-weighted leader selection (StdRng seeded from SHA-256(epoch_number)) |
| `strum` | 0.27.1 | Enum reflection |
| `chrono` | 0.4.41 | Timestamps |

## Architecture

### Module Map

| Module | Responsibility |
|--------|---------------|
| `node` | Node types (Full/Light), registry types (Committer, Sentinel, Executor, Finalizer, Archiver), registration protocol |
| `node::registry` | `NodeRegistry` — DashMap-backed per-type collections, connection management, broadcast |
| `conns` | Network traits, TCP/Unix domain socket implementations, length-prefixed framing |
| `conns::factories` | `ConnFactory` — creates Senders, Listeners, Connections |
| `conns::senders` | `Sender` trait with `TcpSender`/`UdsSender` |
| `conns::streams` | `Stream` trait (sync) + async `StreamReader`/`StreamWriter` via Tokio |
| `conns::listeners` | `Listener` trait with `CoreTcpListener`/`CoreUdsListener` |
| `server` | `ThreadPool` — hybrid sync+async worker pool |
| `config` | `Config` — loads `config.json` + per-environment specs from `/env/` |
| `environment` | `EnvironmentMetadata` — quorum settings, crypto provider, block validators, gas cost model |
| `data` | `DataProvider` trait — abstracts external data store via MsgPack over TCP/UDS |
| `crypto` | `AsymCryptoProvider` (Ed25519 sign/verify, hybrid AES-GCM encrypt), `HashProvider` (SHA-256) |
| `encoding` | JSON and MsgPack serialization helpers |
| `tokens` | `Token`, `BlockValidator` trait, `TokenFactory` (minting), `Blockchain` |
| `blocks` | `Block` and `Blockchain` — append-only chain with hash chaining |
| `transactions` | `Transaction`, `SignedTransaction`, `TransactionCommit`, `TransactionPool`, explicit state machine |
| `validation` | `TransactionValidationSpec` and `BlockValidatorSpec` traits, SelfSigned/Executed specs, spec registries |
| `registry` | `PendingTransactionRegistry` (DashMap-backed tx CRUD + state transitions), `TransactionSignatureRegistry` |
| `epoch` | `Epoch`, `StakeSet`, `LeaderSelector`, `BlockProposer`, `EpochBoundaryDetector`, `resolve_block_conflict()` |
| `gossiper` | Message deduplication (TTL cache) + fan-out to multiple handlers |
| `messages` | Wire message format (`Message` struct), ack/reject helpers |
| `logging` | `Logger` trait with `FileLogger` (file-locked append writes) |
| `user` | `User` struct with `fuel_balance` and `stake` |
| `action_router` | `IActionRouter` trait — routes actions (Process, Preload, Sign, Confirm, etc.) with gas/stake/nonce checks |
| `errors` | `PneumaticError` enum, `ValidationFailureReason`, `TransactionRiskFactor`, `ReconciledSignatures` |

### Node Types

| Type | Role |
|------|------|
| **Sentinel** | Gatekeeper — receives raw transactions, validates, routes to executor or direct-to-committer for self-signed tokens |
| **Executor** | Contract execution — fetches data, runs contract logic, hashes results, sends to finalizer |
| **Finalizer** | Quorum orchestration — collects executor signatures, builds blocks, dispatches to committers |
| **Committer** | Terminal node — commits blocks to token blockchains, manages epoch transitions, staking, leader selection |
| **Archiver** | Block distribution recipient |

## Consensus Flow

The protocol processes a transaction through a multi-stage pipeline. The path depends on whether the token uses self-signed or executed validation:

```
Sender → Sentinel → ──────────────────────────────────────────────────────────→ Committer
                        │
                        ├─ SelfSigned token: Sentinel validates → Committer (skips Executor + Finalizer)
                        │
                        └─ Standard token:
                            Sentinel → Executor (preload + execute + hash) →
                            Finalizer (collect signatures → quorum → build block) →
                            Committer (commit block to chain)
```

### Transaction State Machine

```
Pending → Preloaded → Validated → Executing → Finalizing → Committed
                                           │
                                           └→ Failed (any stage)
```

Each state transition is explicit via the `TransactionState` enum. A `PendingTransaction` holds an atomic lock count to prevent premature collection during multi-stage transit.

### Epoch-Based Consensus

- Time-bounded epochs where a single leader produces blocks
- Leader selected via **stake-weighted deterministic** selection — `SHA-256(epoch_number)` seeds `StdRng`, sorted stake walk
- `EpochBoundaryDetector` detects expired epochs and stale blocks
- `resolve_block_conflict()` resolves conflicting proposals: higher stake wins; tie-break by lexicographic hash comparison

### Gas Model

- `CostModel` defines `base_cost`, `global_min_stake`, `admin_public_key`, `admin_tax_percentage`
- `verify_gas()` checks user `fuel_balance` against gas cost before execution
- Per-action multipliers are planned (currently flat `base_cost`)

### Cryptography

- **Signatures**: Ed25519 via `ed25519-dalek` — 32-byte keys, constant-time, no padding oracle risk
- **Hashing**: SHA-256 via `ring`
- **Encryption**: Hybrid AES-256-GCM + X25519 key exchange
  - Self-encryption: each call generates ephemeral keypair, derives shared secret, encrypts
  - Cross-recipient: encrypt to arbitrary recipient's X25519 public key
  - Wire format: `[32-byte ephemeral PK][ciphertext + 16-byte GCM tag]`

### Wire Protocol

Data frames: 4-byte big-endian length header + MsgPack-serialized payload. Read in two steps — first 4 bytes for length, then read that many bytes. Each node registry type has dedicated external and internal port numbers (e.g., Committer = 42001 external, 50000 internal).

## Sub-Crate Details

### pneumatic_sentinel

Transaction validation and routing node. Handles actions: `Process`, `Confirm`, `Reject`, `Register`, `Clear`.

- **PendingTransactionRegistry**: DashMap-backed concurrent registry for transaction CRUD with lock-based state management
- **TransactionValidator**: Loads token from `DataProvider`, delegates to spec-based validation
- **Gossiper**: Message dedup (TTL cache) + signature verification + fan-out to handlers

**Test count**: 12

### pneumatic_executor

Contract execution with configurable backpressure. Receives preloaded transactions, executes contract logic, returns hashed results.

- **Backpressure**: `max_in_flight` limits concurrent executions; rejects when at capacity
- **Execution task**: Fetches contract/user data → executes contract → hashes result → transitions to Finalizing → sends to Finalizer
- **Stub**: Contract execution currently returns serialized transaction as output

**Test count**: 9

### pneumatic_finalizer

Decomposed from a monolithic C# design into three focused components:

| Component | Responsibility |
|-----------|---------------|
| `SignatureCollector` | Collects/verifies executor signatures, checks quorum (supermajority, stake-weighted conflict resolution) |
| `BlockBuilder` | Builds `SignedTransaction` and `Block` from reconciled signatures, signs with finalizer key |
| `MessageDispatcher` | Sends blocks to committers, clear notifications to sentinels |

**Test count**: 22

### pneumatic_committer

Terminal node — commits validated blocks, manages epochs and staking.

| Component | Responsibility |
|-----------|---------------|
| `Committer` | Receives `TransactionCommit`, validates/env-checks, commits blocks, distributes to archivers |
| `BlockServices` | Token block commitment, block distribution |
| `StakeStore` | In-memory stake tracking |
| `StakingManager` | Applies staking ops (stubbed — no persistence) |
| `EpochReconciler` | Chain analysis at epoch boundaries (stubbed — returns empty) |
| `LeaderSelector` | Stake-weighted leader selection (replaced stub with real implementation) |

## Development

### Adding a New Validation Spec

1. Implement `TransactionValidationSpec` trait (with `validate()`, `calculate_risk()`, `name()`)
2. Implement `BlockValidatorSpec` trait for block-level validation
3. Register via `ValidationSpecRegistry::register()` or `register_defaults()`

### Adding a New Node Type Handler

1. Add the action string to the `match` in the relevant node's `handle_*` method
2. Implement the handler — register transaction, transition state, route via gossiper
3. Add tests covering success and error paths

### Testing Conventions

- Inline `#[cfg(test)] mod tests` blocks in every source file
- Factory helpers follow `make_*` pattern
- Concurrent tests use `std::thread::spawn` with `Arc`-shared DashMaps
- `StubDataProvider` for unit tests (in-memory, pre-loaded data)
- Test filter: `cargo test <module>::tests::<name>`

### Running All Tests

```bash
cargo test --workspace --lib
# 303 tests: 237 core + 22 finalizer + 25 sentinel + 9 executor + 10 committer
```

---

## Roadmap to Production Deployment

This roadmap tracks the work from current foundation state through a production-ready deployment. It maps directly to the implementation checklist in [TASKS.md](TASKS.md).

### Phase 0: Foundation ✅

**Status: COMPLETE** — 303 tests passing across 5 crate targets (237 core + 25 sentinel + 22 finalizer + 9 executor + 10 committer), all core types and traits implemented.

- Workspace structure, error types, transaction state machine, crypto provider, validation spec system, registries, gossiper, action router, epoch types
- BlockProposer, LeaderSelector, EpochBoundaryDetector, conflict resolution
- TokenFactory minting, Token data CRU operations, per-action gas cost modeling, transaction gas deduction/persistence
- Node registry type selection, registration handling, environment metadata specs, TcpConnection graceful shutdown
- Sub-crates: sentinel, executor, finalizer, committer all build and test

---

### Phase 1: Sentinel Node Integration (Priority: HIGH)

**Goal**: Make the sentinel node functional — receive, validate, and route real transactions.

| Task | Description | Estimate |
|------|-------------|----------|
| Wire `initialize()` | Create closure calling `self.on_data_received(raw)` and pass to `gossiper.initialize()` | 2h |
| `handle_process_request` | Implement preload → validate → assign finalizer flow (refs: C# Sentinel.cs:131-175) | 8h |
| `process_transaction` | Full transaction processing: register → preload → spec validation → route | 8h |
| `handle_confirmation` | Acquire transaction, verify finalizer, transition to Committed, notify sentinels | 4h |
| `handle_rejection` | Check awaiting_finalizer state, pick new finalizer via risk-based selection, reassign | 4h |
| `handle_register_request` | Deserialize `NodeRegistryRequest`, validate stake, register node | 4h |
| `handle_clear_request` | Already implemented — deserialize tx_id, remove from registry | 0h (done) |
| Risk-based routing | Route higher-risk transactions to more finalizers; adjust quorum dynamically | 6h |
| `send_to_executor_for_preload` | Use `TransactionNotifier` to send Preload action to Executor nodes | 4h |
| `TransactionValidator` | Implement `validate_transaction` with spec lookup, `calculate_risk` concrete impl | 6h |
| `TransactionNotifier` | Create module; implement `send_to_nodes` using `NodeRegistry` to look up + send to target type | 6h |

**Sub-total**: 52h / ~1 week

---

### Phase 2: Executor Contract Execution (Priority: HIGH)

**Goal**: Replace stub contract execution with real computation.

| Task | Description | Estimate |
|------|-------------|----------|
| Execute contract bytecode | Decode contract data (bytecode/ABI), run with transaction payload, return computed output | 12h |
| Build execution result | Proper result structure with transaction id, computed data, result hash | 4h |
| Wire `validate_execution_result` | Call after `execute_contract`; use result to transition to Finalizing state | 4h |
| Wire `get_finalizer_key` | Use assigned finalizer key from validation result when sending to finalizer | 2h |
| Result serialization | Define wire format for execution result (MsgPack struct) | 4h |

**Sub-total**: 26h / ~3 days

---

### Phase 3: Finalizer Pipeline Completion (Priority: HIGH)

**Goal**: Wire all finalizer components end-to-end.

| Task | Description | Estimate |
|------|-------------|----------|
| Wire `initialize()` | Subscribe to "Preload" and "Sign" actions via Gossiper message router | 4h |
| Fill `try_finalize` stake/voter fields | Get `total_stake`/`total_voters` from `EnvironmentMetadata` instead of hardcoded 0 | 2h |
| Wire `previous_hash` | Get actual chain state's last hash from token's blockchain | 4h |
| `SignatureCollector.reconcile_signatures` | Implement stake-weighted conflict resolution (supermajority vote) | 6h |
| Message dispatcher | Use registered connections instead of `NodeRegistry.send_to_all` stub | 4h |
| Shutdown handling | Proper drain of in-flight tasks on shutdown | 2h |

**Sub-total**: 22h / ~3 days

---

### Phase 4: Committer Node Completion (Priority: MEDIUM)

**Goal**: Full commit + epoch management pipeline.

| Task | Description | Estimate |
|------|-------------|----------|
| `TokenFactory::mint_token` | Charge minting fee from `ProtocolUser.fuel_balance`, calculate fee = base_cost × 10, deduct via data_provider, record admin tax | 6h |
| EpochReconciler chain analysis | Detect misshapen tokens, finalization conflicts at epoch boundaries | 12h |
| StakingManager persistence | Persist AddStaker/RemoveStaker/Slash/Reward ops to data store | 8h |
| Gas deduction | Deduct `gas_used` from user `fuel_balance` after successful execution in committer pipeline | 6h |
| Per-action gas cost | Add `amount_multiplier: HashMap<String, f64>` to `CostModel`, compute `gas_used = base_cost + (amount × multiplier)` | 6h |
| `NodeRegistry.send_to_all` | Use registered connections (`NodeRegistryNode.conn.send()`) instead of creating senders on the fly | 6h |
| `NodeRegistry.process_registration` | Iterate Add/Remove batch, insert/remove from DashMap, validate entries | 4h |
| `check_and_commit_transaction_results` | Add Result propagation (no silent logger.log failures) | 2h |

**Sub-total**: 50h / ~1 week

---

### Phase 5: Server & Infrastructure (Priority: MEDIUM)

**Goal**: Fix server bugs, improve connection management.

| Task | Description | Estimate |
|------|-------------|----------|
| Server worker loop | Remove `return` in `Worker::get_sync_thread` — loop must continue processing jobs | 2h |
| Async poison test | Fix hanging test — needs `catch_unwind` or separate tokio runtime | 4h |
| TcpConnection Drop impl | Cancel `listening_thread` and join with timeout on drop | 4h |
| Config node type selection | Parse config spec for node type selection and stake requirements | 4h |
| EnvironmentMetadataSpec wire-up | Wire `allowed_token_types`, `trans_validation_specs`, `block_validation_specs`, `sym_crypto_provider` fields | 6h |
| Token.get_asset_mut | Return `&mut Option<Vec<u8>>` or add `set_asset` method | 2h |

**Sub-total**: 22h / ~3 days

---

### Phase 6: Test Coverage Expansion (Priority: MEDIUM)

**Goal**: Close remaining test gaps across all modules.

| Module | Current | Target | Gap |
|--------|---------|--------|-----|
| `crypto.rs` | Partial | Full | HashProvider tests, crypto round-trip encrypt/decrypt | 6h |
| `blocks.rs` | 6 tests | 10+ | Chain validation edge cases, BlockFactory hash determinism | 4h |
| `config.rs` | 0 tests | 5+ | Config loading, environment spec parsing | 4h |
| `data.rs` | Stub only | 8+ | DefaultDataProvider wire format, StubDataProvider scenarios | 4h |
| `tokens.rs` | 0 tests | 8+ | Token creation, comparison, minting | 4h |
| `server.rs` | 1 (broken) | 5+ | ThreadPool lifecycle, async job handling, shutdown | 6h |
| `epoch.rs` | 23 tests | 30+ | EpochReconciler integration, StakeSet edge cases, deterministic leader (SA_02 done) | 4h |
| `registry.rs` | 33 core + 11 concurrent | 50+ | More concurrent stress tests | 6h |
| `validation.rs` | 17 tests | 25+ | Custom spec registration, multi-token validation | 4h |
| Integration | 1 (self-signed) | 5+ | Full pipeline: process → validate → execute → finalize → commit; ~~wire framing socket round-trip (SA_01 companion)~~ 6 more conns integration tests added | 10h |

**Sub-total**: 56h / ~1 week

---

### Phase 7: Production Readiness (Priority: LOW — Post-MVP)

**Goal**: Security hardening, observability, deployment infrastructure.

| Area | Tasks |
|------|-------|
| **Security** | ~~Wire framing fix (SA_01)~~, ~~deterministic leader election (SA_02)~~, ~~nonce validation (SA_03)~~, ~~DH-to-AES KDF + random nonce (SA_04)~~, ~~panic-free error returns (SA_05)~~, ~~deterministic gas math (SA_06)~~, ~~enum rename (SA_07)~~, ~~max frame size limit (SA_08)~~, key rotation, rate limiting, circuit breaker, input size limits on MsgPack frames |
| **Observability** | Structured logging (json), Prometheus metrics (tx throughput, epoch duration, quorum latency), distributed tracing |
| **Deployment** | Docker compose for multi-node testnet, health check endpoints, graceful shutdown with task drain |
| **Networking** | TLS for TCP connections (SA_09), connection pooling, reconnection logic for dropped peers |
| **Data Layer** | Persistent data store backend (replace `DefaultDataProvider` TCP stub with real DB), backup/restore for token state |
| **Testing** | Chaos testing (network partitions, node crashes), load testing (tx/s throughput), fuzz testing on MsgPack deserialization, integration tests exercising real socket paths (SA_01 companion test) |
| **Documentation** | API documentation (rustdoc), architecture decision records (ADRs), runbook for operators |

---

### Phase 8: Security Audit Remediation (Priority: HIGH — Pre-release)

**Goal:** Fix all blocking and high-severity findings from the 2026-08-11 external audit. This phase MUST complete before any testnet deployment.

| # | Finding | Severity | File | Effort |
|---|---------|----------|------|--------|
| SA_01 | ~~Fix wire framing buffer: `vec![0u8, 4]` → `u32::from_be_bytes([0u8; 4])`~~ | Critical | `src/conns.rs:37,51` | ~~2h~~ |
| SA_02 | ~~Deterministic leader election~~ — seeded StdRng from SHA-256(epoch_number), sorted stake walk | Critical | `src/epoch.rs:154-186` | ~~6h~~ 2h |
| SA_03 | ~~Extract real nonce from transaction instead of hardcoded `0`~~ — deserialize `Transaction` from `message.body`, extract `sequence_number` (nonce) and `amount` | Critical | `src/action_router.rs` | ~~2h~~ 1h |
| SA_04 | ~~Add HKDF between DH output and AES key; use random 96-bit nonce (not zero)~~ — `derive_aes_key()` via HKDF-SHA256, `generate_nonce()` via `getrandom`, wire format `[32-byte PK][12-byte nonce][ciphertext + tag]` | Critical | `src/crypto.rs` | ~~4h~~ 2h |
| SA_05 | ~~Replace `.expect()` / `panic!` on network paths with `Result` error returns~~ — `PneumaticError::CryptoError`, `ConnError::DecryptError`, `DataError::CryptoError`, `Display` impls, atomic `add_transaction` | High | `crypto.rs`, `errors.rs`, `data.rs`, `registry.rs` | ~~4h~~ 1h |
| SA_06 | ~~Integer fixed-point gas math — no `f64` in consensus-relevant computation~~ | High | `src/action_router.rs` | ~~2h~~ 30min |
| SA_07 | ~~Rename `AsymCryptoProviderType::RSA` → `Ed25519`~~ | Medium | `src/crypto.rs` | ~~1h~~ |
| SA_08 | ~~Max frame size limit (16 MB) before `vec!` allocation~~ | Medium | `src/conns.rs` | 1h |
| SA_09 | TLS for TCP connections (rustls) | Medium | `conns::listeners`, `conns::factories` | 8h |

**Sub-total**: 25h / ~3 days (SA_01 + SA_02 + SA_03 + SA_04 + SA_05 + SA_06 + SA_07 + SA_08 complete — 9h saved)

**Tracking:** Full audit remediation plan with code-level details in [TASKS.md](TASKS.md) section "Security Audit Remediation".

---

### Summary: Effort Estimates

| Phase | Effort | Blockers Previous Phase |
|-------|--------|------------------------|
| 0. Foundation | ✅ Done | — |
| 1. Sentinel Integration | ~1 week | Phase 0 |
| 2. Executor Execution | ~3 days | Phase 1 (Partial — can run in parallel with Sentinel wiring) |
| 3. Finalizer Completion | ~3 days | Phase 1, 2 |
| 4. Committer Completion | ~1 week | Phase 1-3 |
| 5. Server & Infra | ~3 days | Can run in parallel with 1-3 |
| 6. Test Coverage | ~1 week | Phase 1-4 |
| 7. Production Readiness | ~4 weeks | Phases 1-6 |

**MVP Total**: ~12 weeks (with parallel work: ~8 weeks)
**Production Total**: ~16 weeks from MVP

---

## Contributing

1. Fork the repository
2. Create a feature branch from `main`
3. Implement changes with inline `#[cfg(test)]` tests
4. Run `cargo test --workspace --lib` — all tests must pass
5. Update TASKS.md for completed items
6. Open a pull request

See [TASKS.md](TASKS.md) for the full implementation checklist with C# reference mappings.
