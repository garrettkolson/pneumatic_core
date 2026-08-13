# pneumatic_core

**Pneumatic** is a Rust implementation of a proof-of-stake blockchain protocol for distributed worker node networks. It provides the full transaction pipeline — from submission through validation, execution, finalization, and commitment — with support for self-signed tokens, stake-weighted leader election, epoch-based consensus, deterministic per-transaction routing with stake snapshots, and hybrid AES-256-GCM encryption.

[![Rust](https://img.shields.io/badge/Rust-2021-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

---

## Quick Start

```bash
cargo check           # Verify compilation
cargo build           # Build all workspace crates
cargo test --workspace --lib   # Run 340 tests across 5 crate targets
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
| `rand` | 0.8 | Deterministic stake-weighted leader selection + per-transaction finalizer routing (StdRng seeded from SHA-256) |
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
| `data` | `DataProvider` trait — abstracts external data store via MsgPack over TCP/UDS; `get_stake_snapshot`/`save_stake_snapshot` for epoch boundary stake snapshots; `StubDataProvider` for unit testing |
| `crypto` | `AsymCryptoProvider` (Ed25519 sign/verify, hybrid AES-GCM encrypt), `HashProvider` (SHA-256) |
| `encoding` | JSON and MsgPack serialization helpers |
| `tokens` | `Token`, `BlockValidator` trait, `TokenFactory` (minting), `Blockchain` |
| `blocks` | `Block` and `Blockchain` — append-only chain with hash chaining |
| `transactions` | `Transaction`, `SignedTransaction`, `TransactionCommit`, `TransactionPool`, explicit state machine, proposer_key for conflict resolution |
| `validation` | `TransactionValidationSpec` and `BlockValidatorSpec` traits, SelfSigned/Executed specs, spec registries |
| `registry` | `PendingTransactionRegistry` (DashMap-backed tx CRUD + state transitions), `TransactionSignatureRegistry` |
| `epoch` | `Epoch`, `StakeSet` (serializable), `deterministic_select()` (per-tx routing), `LeaderSelector`, `BlockProposer`, `EpochBoundaryDetector`, `resolve_block_conflict()`, `CandidateRegistry`, `IEpochReconciler`, `IStakingManager`, `IEpochLeaderSelector`, `IBlockProposer` |
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
| **Finalizer** | Quorum orchestration — collects executor signatures **for conflict resolution only**; single-finalizer dispatch for optimistic commit in the happy path; deterministic per-transaction routing via stake snapshots; tracks epoch number for block creation |
| **Committer** | Terminal node — commits blocks to token blockchains, manages epoch transitions, staking, leader selection |
| **Archiver** | Block distribution recipient |

---

## Architecture Design Decisions

### ADR-001: Trait-Based Abstraction Over Inheritance

All pluggable components use Rust traits with concrete implementations rather than inheritance hierarchies. Examples: `Connection`, `Sender`, `Stream`, `Listener`, `DataProvider`, `BlockValidator`, `Logger`, `AsymCryptoProvider`, `HashProvider`, `IActionRouter`.

**Rationale**: Rust lacks inheritance; traits provide zero-cost abstractions and allow any concrete type to satisfy an interface. This makes testing straightforward (`StubDataProvider`, `StubLeaderSelector`) and allows swapping implementations without refactoring consumers.

### ADR-002: DashMap for Concurrent Registry State

Node registries (`NodeRegistry`, `CandidateRegistry`, `PendingTransactionRegistry`, `StakeStore`) all use `DashMap` as their backing store.

**Rationale**: Lock-free concurrent HashMap provides better throughput than `Mutex<HashMap>` for read-heavy workloads. All registries are keyed by `Vec<u8>` (public key bytes) and accessed from multiple async tasks. DashMap's per-shard locking avoids global contention.

### ADR-003: Deterministic Leader Election via Seeded RNG

`LeaderSelector::select()` seeds a `StdRng` with `SHA-256(epoch_number.to_be_bytes())` and walks a **sorted** stake set.

**Rationale**: `rand::thread_rng()` is non-reproducible — two nodes with identical state would pick different leaders, making consensus impossible. The SHA-256 seed + sorted walk is a pure function: identical `StakeSet` + `epoch_number` always yields the same leader. Seed is later upgraded to include `prev_block_hash` for forward-security.

### ADR-004: Deterministic Per-Transaction Routing (Not Epoch-Wide Leader)

Each transaction is routed to a specific finalizer via `deterministic_select(stakers, seed_bytes, epoch_number)` where `seed_bytes = tx_id_bytes`. A stake snapshot frozen at the epoch boundary is used as the selection authority.

**Rationale**: An epoch-wide leader is a throughput bottleneck — only one node proposes blocks per epoch. Per-transaction routing distributes work across all staked nodes. The stake snapshot eliminates state divergence: all nodes agree on the selection authority because it's persisted in `DataProvider` at epoch boundaries. The three-tier cache (local → DataProvider → peer) minimizes network latency for the common case (local cache hit).

### ADR-005: Optimistic Finality with Conflict-Only Voting

Standard tokens commit immediately after single-executor execution + single-finalizer signature. The 2/3 quorum machinery is repurposed for conflict resolution only — invoked when `CandidateRegistry` detects a genuine fork (two proposers building on the same parent).

**Rationale**: In the vast majority of cases, no fork occurs. Requiring 2/3 quorum in the happy path wastes bandwidth and latency. The quorum protocol remains available to resolve genuine conflicts, where its safety guarantees matter. Blocks start as `Optimistic` and are upgraded to `Confirmed` after a time-based or depth-based guarantee with no observed conflict.

### ADR-006: Stake Snapshots Persisted in DataProvider (Not Blocks)

Stake snapshots are stored in `DataProvider` (an abstracted external store), not embedded in block headers.

**Rationale**: Embedding snapshots in blocks would bloat block size and make the chain state dependent on stake topology. DataProvider is already the shared state layer between nodes — it's the natural place for epoch-level state. Nodes recover snapshots on demand from the sentinel's cache hierarchy, with the primary being a local in-memory cache populated from the first block of a new epoch.

### ADR-007: Sentinel as the Routing Authority, Not a Consensus Node

The sentinel performs deterministic finalizer assignment but does not participate in consensus. It does not store chain state or vote on block validity.

**Rationale**: Sentinel is a routing proxy, not a consensus participant. This separation of concerns means the sentinel can be replaced or scaled independently of the consensus protocol. The sentinel's only state is the stake snapshot cache, which is cheap to maintain and easy to recover.

---

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
- **Optimistic finality:** Standard tokens commit immediately after single-executor execution + single-finalizer signature. The 2/3 quorum requirement is repurposed for conflict-resolution only — invoked when `CandidateRegistry` detects a genuine fork. Blocks start as `Optimistic` and become `Confirmed` after N seconds with no observed conflict.

### Deterministic Per-Transaction Routing

A stake snapshot frozen at each epoch boundary enables each transaction to be routed to its own deterministic finalizer, eliminating the epoch-wide leader bottleneck and enabling parallel transaction processing.

- **Snapshot model:** At epoch transitions, the Committer saves a frozen `StakeSet` via `DataProvider`. All nodes can recover it for deterministic routing.
- **Selection function:** `deterministic_select(stakers, seed_bytes, epoch_number)` — seeds a `StdRng` with `SHA-256(epoch_number || seed_bytes)`, then walks the sorted stake set to pick a finalizer. Identical algorithm to epoch leader selection, but per-transaction seed gives per-transaction variation.
- **Three-tier cache** (sentinel): (1) Local cache loaded when first block of new epoch is seen — O(1), no network; (2) DataProvider call (~1ms); (3) Peer request from `NodeRegistry` — reserved.
- **Per-tx assignment:** `assign_finalizer_deterministic()` routes each transaction to its assigned finalizer. On rejection, reassignment uses the same function with a retry suffix to pick a different finalizer.
- **Epoch tracking:** Block and Finalizer both track `epoch_number: u64`. Sentinel assignment, finalizer block creation, and epoch snapshots all use a consistent epoch coordinate.

### Gas Model

- `CostModel` defines `base_cost`, `global_min_stake`, `admin_public_key`, `admin_tax_percentage`
- `verify_gas()` checks user `fuel_balance` against gas cost before execution
- Per-action multipliers are planned (currently flat `base_cost`)

### Cryptography

- **Signatures**: Ed25519 via `ed25519-dalek` — 32-byte keys, constant-time, no padding oracle risk
- **Hashing**: SHA-256 via `ring`
- **Encryption**: Hybrid AES-256-GCM + X25519 key exchange
  - Self-encryption: each call generates ephemeral keypair, derives shared secret via DH, derives AES key via HKDF-SHA256, encrypts with random 96-bit nonce
  - Cross-recipient: encrypt to arbitrary recipient's X25519 public key
  - Wire format: `[32-byte ephemeral PK][12-byte nonce][ciphertext + 16-byte GCM tag]`

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

Epoch tracking: `Finalizer` tracks `current_epoch` for block creation. `BlockBuilder::create_block()` accepts `epoch_number` for hash-chain integrity.

**Test count**: 26

### pneumatic_committer

Terminal node — commits validated blocks, manages epochs and staking.

| Component | Responsibility |
|-----------|---------------|
| `Committer` | Receives `TransactionCommit`, validates/env-checks, commits blocks, distributes to archivers |
| `BlockServices` | Token block commitment, block distribution |
| `StakeStore` | In-memory stake tracking |
| `StakingManager` | Applies staking ops (stubbed — no persistence) |
| `EpochReconciler` | Same-chain fork detection via `CandidateRegistry`, stake resolution from `StakeStore` (Phase 2) |
| `LeaderSelector` | Stake-weighted leader selection (replaced stub with real implementation) |
| Epoch snapshot persistence | `handle_epoch_reconcile` and `advance_epoch` save frozen `StakeSet` via `DataProvider` for sentinel deterministic routing |

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
# 340 tests: 256 core + 32 sentinel + 26 finalizer + 9 executor + 17 committer
```

---

## Roadmap to Production Deployment

This roadmap tracks the work from current foundation state through a production-ready deployment. It maps directly to the implementation checklist in [TASKS.md](TASKS.md).

### Phase 0: Foundation ✅

**Status: COMPLETE** — 340 tests passing across 5 crate targets (256 core + 32 sentinel + 26 finalizer + 9 executor + 17 committer), all core types and traits implemented.

- Workspace structure, error types, transaction state machine, crypto provider, validation spec system, registries, gossiper, action router, epoch types
- BlockProposer, LeaderSelector, EpochBoundaryDetector, conflict resolution
- TokenFactory minting, Token data CRU operations, per-action gas cost modeling, transaction gas deduction/persistence
- Node registry type selection, registration handling, environment metadata specs, TcpConnection graceful shutdown
- Sub-crates: sentinel, executor, finalizer, committer all build and test

### Phase 5: Deterministic Per-Transaction Routing ✅

**Status: COMPLETE** — 2026-08-12 — 18 new tests (5 deterministic_select, 4 stake_snapshot_cache, 2 block_builder, 2 message_dispatcher, 5 other).

- `StakeSet` made serializable; `DataProvider` gained `get_stake_snapshot`/`save_stake_snapshot` methods
- `deterministic_select(stakers, seed_bytes, epoch_number)` pure function — seeded SHA-256, sorted stake walk
- `StakeSnapshotCache` in sentinel: 3-tier (local → DataProvider → peer)
- `assign_finalizer_deterministic()` + `assign_finalizer_deterministic_retry()` wired into sentinel routing
- `Block.epoch_number: u64` added and propagated through all constructors and call sites
- `Committer::handle_epoch_reconcile` + `advance_epoch` persist stake snapshots
- `Finalizer` tracks `current_epoch` for block creation

---

### Phase 1: Sentinel Node Integration (Priority: HIGH)

**Goal**: Make the sentinel node functional — receive, validate, and route real transactions.

| Task | Description | Estimate | Status |
|------|-------------|----------|--------|
| Wire `initialize()` | Create closure calling `self.on_data_received(raw)` and pass to `gossiper.initialize()` | 2h | **DONE** |
| `handle_process_request` | Implement preload → validate → assign finalizer flow (refs: C# Sentinel.cs:131-175) | 8h | **DONE** |
| `process_transaction` | Full transaction processing: register → preload → spec validation → route | 8h | Open |
| `handle_confirmation` | Acquire transaction, verify finalizer, transition to Committed, notify sentinels | 4h | **DONE** |
| `handle_rejection` | Check awaiting_finalizer state, pick new finalizer via deterministic assignment, reassign | 4h | **DONE** |
| `handle_register_request` | Deserialize `NodeRegistryRequest`, validate stake, register node | 4h | **DONE** |
| `handle_clear_request` | Already implemented — deserialize tx_id, remove from registry | 0h | **DONE** |
| Risk-based routing | Route higher-risk transactions to more finalizers; adjust quorum dynamically | 6h | Open |
| `send_to_executor_for_preload` | Use `TransactionNotifier` to send Preload action to Executor nodes | 4h | **DONE** |
| `TransactionValidator` | Implement `validate_transaction` with spec lookup, `calculate_risk` concrete impl | 6h | **DONE** |
| `TransactionNotifier` | Create module; implement `send_to_nodes` using `NodeRegistry` to look up + send to target type | 6h | **DONE** |
| `StakeSnapshotCache` | Three-tier stake snapshot cache for sentinel deterministic routing | 4h | **DONE** (Phase 5) |
| `assign_finalizer_deterministic` | Deterministic finalizer assignment with retry suffix for rejection | 2h | **DONE** (Phase 5) |

**Sub-total**: 64h / ~1 week — 8 tasks remaining (can continue in parallel with Phase 2)

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

| Task | Description | Estimate | Status |
|------|-------------|----------|--------|
| Wire `initialize()` | Subscribe to "Preload" and "Sign" actions via Gossiper message router | 4h | Open |
| Fill `try_finalize` stake/voter fields | Get `total_stake`/`total_voters` from `EnvironmentMetadata` instead of hardcoded 0 | 2h | Open |
| Wire `previous_hash` | Get actual chain state's last hash from token's blockchain | 4h | Open |
| `SignatureCollector.reconcile_signatures` | Implement stake-weighted conflict resolution (supermajority vote) | 6h | **DONE** |
| Message dispatcher | Use registered connections instead of `NodeRegistry.send_to_all` stub | 4h | **DONE** |
| Shutdown handling | Proper drain of in-flight tasks on shutdown | 2h | **DONE** |
| Epoch tracking | `Finalizer.current_epoch` field, `advance_epoch()` accessor, wire into block creation | 4h | **DONE** (Phase 5) |

**Sub-total**: 26h / ~3 days — 3 tasks remaining

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

### Phase 5: Optimistic Finality (Priority: HIGH)

**Goal:** Replace per-transaction blocking quorum with conflict-only voting; enable instant finality in the happy path.

| Task | Description | Estimate | Status |
|------|-------------|----------|--------|
| Lock design decisions | Resolve 4 Phase-0 questions (see TASKS.md §Protocol Rearchitecture Phase 0): conflict definition, quorum scope, voting weight pool, losing block behavior | 4h | **Resolved** — see design decisions below |
| Add `CandidateRegistry` | DashMap-backed `(token_id, previous_hash) → Vec<(Block, proposer_key)>` keyed candidate store | 8h | **DONE** |
| Add `finality_status` to `Block` | `Optimistic` vs `Confirmed` enum; downstream consumers check status | 4h | **DONE** |
| Add proposer public key to `Block`/`SignedTransaction` | Explicit proposer key for conflict resolution stake lookup | 4h | **DONE** |
| Replace `EpochReconciler::reconcile_internal()` | Same-chain conflict detection via `CandidateRegistry`; fill `stake_a`/`stake_b` from `StakeStore` | 12h | **DONE** |
| Wire `resolve_block_conflict()` into commit path | On detection, commit winner, drop loser, optionally slash double-proposers, broadcast via gossiper | 8h | Open |
| Replace quorum gate with optimistic path | One Executor executes → one Finalizer signs/dispatches → Committer commits as `Optimistic`; quorum machinery repurposed for conflict-only resolution | 16h | **DONE** |
| Add vote/dispute message types | New `Message` variant for "I saw candidate block" and "I vote for block X" | 8h | Open |
| Conflict-vote aggregation | `SignatureCollector`-like struct scoped to conflicts rather than per-transaction quorum | 8h | Open |
| Conflict scenario tests | Two proposers, same `previous_hash` → `CandidateRegistry` catch → `resolve_block_conflict` → hash tie-break | 8h | Open |
| Concurrency + e2e pipeline tests | Submit → optimistic → no conflict → confirmed; submit → conflict → resolved → slashing | 12h | Open |

**Sub-total**: ~72h / ~2 weeks remaining

### Phase 5b: Deterministic Per-Transaction Routing (NEW — Completed)

| Task | Description | Estimate | Status |
|------|-------------|----------|--------|
| Snapshot Model | `StakeSet` serializable, `DataProvider` snapshot methods | 8h | **DONE** |
| Selection Function | `deterministic_select()` pure function — seeded SHA-256, sorted stake walk | 4h | **DONE** |
| Stake Snapshot Cache | 3-tier cache (local → DataProvider → peer) in sentinel | 6h | **DONE** |
| Deterministic Assignment | `assign_finalizer_deterministic()` + retry suffix for rejections | 4h | **DONE** |
| Epoch on Block | `Block.epoch_number: u64` field, propagate through constructors | 4h | **DONE** |
| Snapshot Persistence | Committer persists stake snapshot at epoch boundaries | 4h | **DONE** |
| Finalizer Epoch Tracking | `Finalizer.current_epoch`, `advance_epoch()` accessor | 4h | **DONE** |

**Sub-total**: 34h / ~1 week — **COMPLETE**

---

### Phase 6: Server & Infrastructure (Priority: MEDIUM)

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

### Phase 7: Test Coverage Expansion (Priority: MEDIUM)

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

### Phase 8: Production Readiness (Priority: LOW — Post-MVP)

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

### Phase 9: Security Audit Remediation (Priority: HIGH — Pre-release)

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
| 1. Sentinel Integration | ~1 week (8 tasks left) | Phase 0 |
| 2. Executor Execution | ~3 days | Phase 1 (Partial — can run in parallel with Sentinel wiring) |
| 3. Finalizer Completion | ~3 days (3 tasks left) | Phase 1, 2 |
| 4. Committer Completion | ~1 week | Phase 1-3 |
| 5. Optimistic Finality | ~2 weeks remaining | Phase 1-4 |
| 5b. Deterministic Routing | ✅ Done (34h, 1 week) | Phase 0 |
| 6. Server & Infra | ~3 days | Can run in parallel with 1-3 |
| 7. Test Coverage | ~1 week | Phase 1-5 |
| 8. Production Readiness | ~4 weeks | Phases 1-7 |

**MVP Total**: ~14 weeks (with parallel work: ~9.5 weeks)
**Production Total**: ~18 weeks from MVP

---

## Contributing

1. Fork the repository
2. Create a feature branch from `main`
3. Implement changes with inline `#[cfg(test)]` tests
4. Run `cargo test --workspace --lib` — all tests must pass
5. Update TASKS.md for completed items
6. Open a pull request

See [TASKS.md](TASKS.md) for the full implementation checklist with C# reference mappings.
