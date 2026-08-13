# Pneumatic Rust Implementation Checklist

Tracks all tasks for implementing the full pneumatic blockchain protocol in Rust. Maps the C# reference `pneuma/` onto the Rust workspace.

**Format:** `- [ ] [task-id] Description — [file] — refs C#:path`

---

## Phase 0: Workspace

- [x] W01 Create workspace Cargo.toml — `Cargo.toml` — structural

## Phase 1: pneumatic_core Core Library

### 1.0 Error Types (Foundation)

- [x] P0_01 Add `PneumaticError` enum covering all failure paths — `errors.rs` — 8 variants + From impls
- [x] P0_02 Add `From<ConnError>` impl to `PneumaticError` — `errors.rs` — maps to `Network(String)` variant; also fixed `ConnError` Display/Debug infinite recursion (self-referential `write!(f, "{}", self)`)

### 1.1 Transaction Lifecycle (Explicit State Machine)

- [x] P1_01 Add `TransactionState` enum (Pending → Preloaded → Validated → [Executing →] Finalizing → Committed/Failed) — `transactions.rs` — all 7 states present
- [x] P1_02 Add `PendingTransaction` with state machine transitions — `transactions.rs` — transition methods for all states
- [x] P1_03 Implement `PendingTransaction::acquire()` / `release()` with lock count — `transactions.rs`

### 1.2 Transaction & SignedTransaction Model

- [x] P1_04 Add `Transaction` struct with all fields — `transactions.rs` — id, action, token_id, bid, sequence_number, sender, receiver, amount, timestamp, result_hash
- [x] P1_05 Add `Bid` struct — `transactions.rs` — bid_expiry, bid_percentage
- [x] P1_06 Add `TransactionValidationResult` — `transactions.rs` — is_valid, risk, failure_reasons, finalizer_public_key
- [x] P1_07 Add `ValidationFailureReason` enum — `errors.rs` — 13 variants
- [x] P1_08 Add concrete `TransactionRiskFactor` with metrics (affected_parties, amount, is_contract, is_multi_party) — `errors.rs`
- [x] P1_09 Refactor `SignedTransaction` to C# signature model — `transactions.rs` — leader/finalizer/executor sigs
- [x] P1_10 Update `TransactionCommit` with serialized block data — `transactions.rs` — proposed_block: Block

### 1.3 Token & Blockchain Refactoring

- [x] P1_11 Add fields to Token (id, sequence_number, is_self_verified, is_non_transferable, block_validation_spec_name, environment_id) — `tokens.rs` — all 10 fields present
- [x] P1_12 Add `Token::create_block(metadata, signed_tx)` — `tokens.rs` — hash chains to previous or genesis
- [x] P1_13 Convert `Token::validate_block` → spec lookup — `tokens.rs` — looks up spec by name from env
- [x] P1_14 Implement `Token::commit_block` — `tokens.rs` — validates, trims, hashes, appends, increments sequence
- [x] P1_15 Add `Blockchain` metadata fields — `blocks.rs` — metadata: HashMap<String, String>
- [x] P1_16 Refactor `Blockchain::create_hash` → HashProvider — `blocks.rs` — BlockFactory calls BasicHashProvider

### 1.4 ValidationSpec System (CRITICAL)

- [x] P1_17 Create `validation.rs` with `TransactionValidationSpec` trait — `validation.rs` — validate(), calculate_risk(), name()
- [x] P1_18 Implement `SelfSignedBlockValidatorSpec` (checks owner signature, enables skip path) — `validation.rs` — implements TransactionValidationSpec
- [x] P1_19 Implement `ExecutedBlockValidatorSpec` — `validation.rs` — validates sender, nonce, gas, amount, risk
- [x] P1_20 Add `transaction_validation_specs` to EnvironmentMetadata — `environment.rs` — registered via load_from_spec + register_defaults()

### 1.5 PendingTransactionRegistry

- [x] P1_21 Create `registry.rs` with `PendingTransactionRegistry` (DashMap-backed) — `registry.rs`
- [x] P1_22 All methods return `Result` (never `Option`) — `registry.rs` — get_validation_result() and get_transaction_mut() now return Result<T, PneumaticError>; all callers updated across 6 files (registry.rs, sentinel.rs, finalizer.rs, executor.rs, committer.rs, validation.rs); test names updated to reflect new semantics (e.g. `get_validation_result_from_pending_returns_none` → `get_validation_result_from_pending_returns_error`)

### 1.6 TransactionSignatureRegistry

- [x] P1_23 Implement `TransactionSignatureRegistry` — `registry.rs` — 7 methods, DashMap-backed

### 1.7 EpochManager Types (Concrete Structure)

- [x] P1_24 Add `Epoch` struct — `epoch.rs` — start/end timestamp, epoch_number, leader_public_key
- [x] P1_25 Add `StakingOp` enum — `epoch.rs` — AddStaker, RemoveStaker, Slash, Reward
- [x] P1_26 Add `EpochReconciliation` struct — `epoch.rs` — misshapen_tokens, conflicts, slashing_ops, reward_ops
- [x] P1_27 Create `IEpochReconciler` trait (returns reconciliation data) — `epoch.rs`
- [x] P1_28 Create `IStakingManager` trait (applies ops) — `epoch.rs`
- [x] P1_29 Create `IEpochLeaderSelector` trait — `epoch.rs`
- [x] P1_30 Stub implementations that return empty structures — `epoch.rs` — StubEpochReconciler, StubStakingManager, StubLeaderSelector

### 1.8 TokenFactory + Token Types

- [x] P1_31 Implement `TokenFactory::mint_token` — `tokens.rs` — generic dispatch on asset type, helpers: mint_user_token, mint_contract_token, mint_proxy_auth_token
- [x] P1_32 Add `SmartContract`, `ContractProxyAuthorization`, `User` structs — `tokens.rs`
- [x] P1_33 Add `ContractToken: Token` — `tokens.rs` — DESIGN DECISION: unified `Token` struct with `asset_data`/`asset_hash` instead of subtype hierarchy
- [x] P1_34 Add `ProxyAuthToken: Token` — `tokens.rs` — same unified approach
- [x] P1_35 Add `UserToken: Token` — `tokens.rs` — same unified approach

### 1.9 Message & BlockValidatorSpec

- [x] P1_36 Add `MessageBody<T>` — `messages.rs` — action: String, body: T
- [x] P1_37 Rename `Message.env_id` → `chain_id` — `messages.rs`
- [x] P1_38 Create `BlockValidatorSpec` trait — `validation.rs` — validate(block, token, env_data)
- [x] P1_39 Add `SelfSignedBlockValidatorSpec` for blocks — `validation.rs` — Implements BlockValidatorSpec: chain integrity + is_self_verified check
- [x] P1_40 Add `ExecutedBlockValidatorSpec` for blocks — `validation.rs` — Implements BlockValidatorSpec: result_hash, executor_sigs, finalizer_sig checks

### 1.10 Gossiper

- [x] P1_41 Implement `Gossiper` struct with config TTL cache — `gossiper.rs` — moka cache with TTL
- [x] P1_42 Fan-out: multiple handler delegates — `gossiper.rs` — `handler: Mutex<Option<...>>` → `handlers: Mutex<Vec<...>>`; `add_handler()` registers extra handlers; `handle_message()` clones raw_data and invokes each handler sequentially; 5 new tests (invokes-all, receives-copy, dedup-skips-all, three-handlers, concurrent-invocation)

### 1.11 IActionRouter

- [x] P1_43 Create `IActionRouter` trait — `action_router.rs` — async route(Message) -> Result<ActionRouterResult>
- [x] P1_44 Implement `ActionRouter` with utility token coordination — `action_router.rs` — implements IActionRouter trait; `handle()` delegates to `route()`; action branches: Process→nonce+gas, Preload→gas+stake(Executor), Sign→stake(Finalizer), Confirm→GasVerified, Reject→NonceUpdated(0), Register→stake(Sentinel), Clear→NonceUpdated(0), DistributeToken→TokenDispatched; utility coordination: `check_nonce()` validates user.nonce against data store, `verify_gas()` checks fuel_balance > 0, `check_stake()` compares fuel_balance against min_stake for node_type; 4 builders (new, new_with_registry, new_with_config); 18 tests
- [x] P1_45 Create `ActionRouterResult` type — `action_router.rs` — 6 variants

### 1.12 Remove Validator

- [x] P1_46 Remove `Validator` from `NodeRegistryType` enum — `node.rs` — enum has Committer, Sentinel, Executor, Finalizer, Archiver

### 1.13 HashProvider

- [x] P1_47 Create `HashProvider` trait — `crypto.rs` — named `HashProvider` (no `I` prefix), fully implemented with SHA-256 via ring

## Phase 2: pneumatic_sentinel Crate

### 2.1 Sentinel

- [ ] P2_01 Create sentinel crate — `sentinel/Cargo.toml` — structural
- [ ] P2_02 Implement `Sentinel` struct — `sentinel/src/sentinel.rs` — refs: C# Sentinel.cs
- [ ] P2_03 Implement `on_data_received`, route by action — `sentinel/src/sentinel.rs` — refs: C# Sentinel.cs:51-79
- [ ] P2_04 Implement `handle_process_request` (preload, validate, assign finalizer) — `sentinel/src/sentinel.rs` — refs: C# Sentinel.cs:131-175
- [ ] P2_05 Implement `handle_confirmation` (state-check, process) — `sentinel/src/sentinel.rs` — refs: C# Sentinel.cs:184-199
- [ ] P2_06 Implement `process_transaction` — `sentinel/src/sentinel.rs` — refs: C# Sentinel.cs:201-215
- [ ] P2_07 Implement `handle_rejection` (reassign finalizer) — `sentinel/src/sentinel.rs` — refs: C# Sentinel.cs:217-229
- [x] P2_08 Implement `handle_register_request` / `handle_clear_request` — `sentinel/src/sentinel.rs` — P2_08 DONE: handle_register_request fully implemented (deserializes NodeRegistryRequest, validates stake, registers node in DashMap); handle_clear_request was already implemented — refs: C# Sentinel.cs:124-129, 231-235
- [ ] P2_09 Risk-based routing (higher risk → more finalizers) — `sentinel/src/sentinel.rs`

### 2.2 TransactionValidator

- [ ] P2_10 Create `transaction_validator.rs` — `sentinel/src/transaction_validator.rs` — refs: C# TransactionValidator.cs
- [ ] P2_11 Implement `validate_transaction` with spec lookup — `sentinel/src/transaction_validator.rs`
- [ ] P2_12 Implement concrete `calculate_risk` — `sentinel/src/transaction_validator.rs`

### 2.3 TransactionNotifier

- [ ] P2_13 Create `transaction_notifier.rs` — `sentinel/src/transaction_notifier.rs` — refs: C# TransactionNotifier.cs
- [ ] P2_14 Implement all notifier methods — `sentinel/src/transaction_notifier.rs`

## Phase 3: pneumatic_executor Crate

### 3.1 Executor

- [x] P3_01 Create executor crate — `executor/Cargo.toml` — structural
- [x] P3_02 Implement `Executor` struct with `max_in_flight` (backpressure) — `executor/src/executor.rs` — refs: C# Executor.cs
- [x] P3_03 Implement `initialize`, `on_data_received` — `executor/src/executor.rs` — refs: C# Executor.cs:27-30, 71-97
- [x] P3_04 Implement `preload_for_transaction` — `executor/src/executor.rs` — refs: C# Executor.cs:33-43
- [x] P3_05 Implement `process_transaction` — `executor/src/executor.rs` — refs: C# Executor.cs:45-67
- [x] P3_06 Implement backpressure check (reject if overloaded) — `executor/src/executor.rs`
- [x] P3_07 Implement preload task cleanup — `executor/src/executor.rs`

## Phase 4: pneumatic_finalizer Crate (Split from C# Monolithic Design)

### 4.1 Finalizer

- [x] P4_01 Create finalizer crate — `finalizer/Cargo.toml` — structural
- [x] P4_02 Implement `Finalizer` struct with SignatureCollector/BlockBuilder/MessageDispatcher — `finalizer/src/finalizer.rs` — refs: C# Finalizer.cs
- [x] P4_03 Implement `initialize`, `handle_preload`, `handle_signature`, `try_finalize` — `finalizer/src/finalizer.rs` — refs: C# Finalizer.cs
- [x] P4_04 Implement shutdown handling — `finalizer/src/finalizer.rs` — refs: C# Finalizer.cs:47-50

### 4.2 SignatureCollector (Split from C# TransactionReconciler)

- [x] P4_05 Create `signature_collector.rs` — `finalizer/src/signature_collector.rs`
- [x] P4_06 Implement `add_signature` — `finalizer/src/signature_collector.rs` — refs: C# TransactionReconciler.cs:63-73
- [x] P4_07 Implement `check_quorum` — `finalizer/src/signature_collector.rs` — refs: C# TransactionReconciler.cs:75-84
- [x] P4_08 Implement `reconcile_signatures` (supermajority, stake-weighted) — `finalizer/src/signature_collector.rs` — refs: C# TransactionReconciler.cs:138-200
- [x] SignatureCollector returns data, does NOT build blocks or send messages

### 4.3 BlockBuilder (Split from C# TransactionReconciler)

- [x] P4_09 Create `block_builder.rs` — `finalizer/src/block_builder.rs`
- [x] P4_10 Implement `build_signed_transaction` — `finalizer/src/block_builder.rs` — refs: C# TransactionReconciler.cs:176-199
- [x] P4_11 Implement `sign_finalizer_block` — `finalizer/src/block_builder.rs` — refs: C# TransactionReconciler.cs:202-225
- [x] P4_12 Implement `create_block` — `finalizer/src/block_builder.rs` — refs: C# TransactionReconciler.cs:96-104

### 4.4 MessageDispatcher (Split from C# TransactionReconciler)

- [x] P4_13 Create `message_dispatcher.rs` — `finalizer/src/message_dispatcher.rs`
- [x] P4_14 Implement `send_to_committers` — `finalizer/src/message_dispatcher.rs` — refs: C# TransactionReconciler.cs:107-111
- [x] P4_15 Implement `send_clear_to_sentinels` — `finalizer/src/message_dispatcher.rs` — refs: C# TransactionReconciler.cs:113-122

## Protocol Rearchitecture: Optimistic Finality

Audited by external review (`PROTOCOL_CHANGES.md`). Maps to README.md Phase 5.

### Phase 0 — Design Decisions (Open TODOs)

Resolve these before writing code — choices will ripple through all phases below.

- [ ] **What exactly triggers "a conflict"?** Formal invariant: two different valid `Block`s that both reference the same `previous_hash` for the same token (i.e., two proposers building on the same parent). Nothing in current code can represent this state yet.
- [ ] **Does the mandatory 2/3 executor quorum go away for standard tokens, or shrink to a minimal-validity check?** Full optimistic finality means most tokens shouldn't need any quorum in the happy path — this is the biggest thing blocking "instant by default."
- [ ] **Is voting weight for conflict resolution the same global `StakeSet` used for epoch leader election, or a logically separate "representative" set?** Nano keeps these distinct (delegated voting weight vs. block production). Currently only have one pool — worth deciding on purpose.
- [ ] **What happens to a losing block/proposer?** Just discarded, or slashed via existing (currently unwired) `StakingOp::Slash`? Real forks often indicate bad-faith double-proposing — worth punishing the latter.

### Phase 1 — Data Model: Represent "Competing Candidates"

- [ ] Add `CandidateRegistry` (DashMap-backed, keyed by `(token_id, previous_hash) → Vec<(Block, proposer_key)>`) — `src/epoch.rs`
- [ ] Add `finality_status` enum (`Optimistic`, `Confirmed`) to `Block` struct — `src/blocks.rs`
- [ ] Add proposer public key to `Block`/`SignedTransaction` if not recoverable from signatures — `src/transactions.rs`, `src/blocks.rs`

### Phase 2 — Replace Conflict Detection Logic ✅ COMPLETE (2026-08-12)

- [x] Replace `EpochReconciler::reconcile_internal()` with same-chain detection — `committer/src/epoch_manager.rs` — replaced cross-token hash comparison with CandidateRegistry-based same-chain fork detection; builds StakeSet from StakeStore for real stake values
- [x] Check `CandidateRegistry` at ingestion time for `(token_id, previous_hash)` collisions — `reconcile_internal()` checks `candidate_count(token_id, tip_hash) >= 2` and reports pairwise conflicts
- [x] Fill `stake_a`/`stake_b` from `StakeStore::get_stake()` instead of hardcoded `0` — both conflict fields resolved from `self.stake_store.get_stake()` per proposer

### Phase 3 — Wire `resolve_block_conflict()` Into Commit Path ✅ COMPLETE (2026-08-13)

- [x] Enrich `resolve_block_conflict()` return type — `src/epoch.rs` — `Result<Vec<u8>>` → `Result<ConflictResolution>` with three outcomes: `DiscardLoser` (network race), `SameProposerSlash` (double-signed), `TieFlagBoth` (tie-break for review)
- [x] Add `CandidateRegistry` to Committer struct — `committer/src/committer.rs` — `Arc<CandidateRegistry>` field, wired through constructor. Both test factories pass shared `Arc` to Committer + EpochReconciler.
- [x] Wire conflict detection into `check_and_commit_transaction_results` — `committer/src/committer.rs` — `handle_conflict_at_commit()` method: before `commit_block()`, check `CandidateRegistry` at `(token_id, previous_hash)`. If conflict, resolve with `resolve_block_conflict()` using real stakes from `StakeStore`. Handle all three outcomes.
- [x] Slash double-proposers at commit time — `committer/src/committer.rs` — `SameProposerSlash` emits `StakingOp::Slash` via `StakingManager.apply_ops()`. Full stake slash amount TBD.
- [x] Broadcast resolution outcome via logging — `committer/src/committer.rs` — all three resolution outcomes logged. Epoch reconciliation will broadcast to archivers via existing `distribute_to_archivers`.
- [x] Test: no conflict → inserts first candidate → normal commit
- [x] Test: conflict, different stakes → DiscardLoser → both candidates tracked
- [x] Test: conflict, same proposer → SameProposerSlash → slash emitted
- [x] Test: conflict, no existing candidates → inserts first → tracked

### Phase 4 — Make Default Path Actually Optimistic

- [ ] For standard tokens: one Executor executes → one Finalizer signs/dispatches → Committer commits as `Optimistic`
- [ ] Quorum/voting in `SignatureCollector` repurposed for conflict-resolution only
- [ ] Define "confirmed" guarantee (e.g., "final after N seconds with no conflict")
- [ ] Expose via `finality_status`

### Phase 5 — Networking Additions

- [ ] Add vote/dispute message type in `messages.rs` — "I saw candidate block" + "I vote for block X"
- [ ] Add conflict-vote aggregation structurally similar to `SignatureCollector` but conflict-scoped — `finalizer/src/`

### Phase 6 — Testing

- [x] Unit tests: conflict detected at commit time, winner by stake, same-proposer slash, equal-stakes tie-break, no-conflict normal path (4 new tests in committer.rs)
- [ ] Concurrency tests: near-simultaneous candidate submission (Arc-shared DashMap)
- [ ] End-to-end pipeline: submit → optimistic → no conflict → confirmed; submit → conflict → resolved → slashing

### Implementation Order Recommendation

Start with **Phase 1 + 2** together (candidate registry + real fork detection) — build and unit-test in isolation without touching the finalizer's quorum behavior. This gives you a correct detector before changing what "instant" means in Phase 4.

## Phase 5: Deterministic Per-Transaction Routing (2026-08-12)

Each transaction gets its own deterministic finalizer via a stake snapshot — eliminates epoch-wide leader bottleneck, enables parallel transaction processing.

### Phase 0: Snapshot Model (pneumatic_core) ✅ COMPLETE

- [x] P5_00_01 Add `Serialize`/`Deserialize` derives to `StakeSet` — `src/epoch.rs`
- [x] P5_00_02 Add `StakeSnapshot(u64)` variant to `GetOp` enum — `src/data.rs`
- [x] P5_00_03 Add `StakeSnapshot(StakeSet)` variant to `SaveOp` enum — `src/data.rs`
- [x] P5_00_04 Add `get_stake_snapshot`/`save_stake_snapshot` to `DataProvider` trait — `src/data.rs`
- [x] P5_00_05 Implement in `DefaultDataProvider` via TCP/UDS — `src/data.rs`
- [x] P5_00_06 Add `stake_snapshots` field + `with_stake_snapshot()` builder to `StubDataProvider` — `src/data.rs`
- [x] P5_00_07 Add stub implementations to committer's `TestDataProvider` — `committer/src/committer.rs`

### Phase 1: Deterministic Selection Function ✅ COMPLETE

- [x] P5_01_01 Extract `deterministic_select(stakers, seed_bytes, epoch_number)` pure function — `src/epoch.rs:100-158`
- [x] P5_01_02 Seed = `SHA-256(epoch_number || seed_bytes)` → `StdRng`, sorted stake walk
- [x] P5_01_03 Refactor `LeaderSelector::select()` to delegate to `deterministic_select` — `src/epoch.rs`
- [x] P5_01_04 Expose via `pub use epoch::deterministic_select` — `src/lib.rs`
- [x] P5_01_05 5 unit tests: empty returns none, single staker deterministic, different txs distribute, cross-epoch determinism, zero stake returns none

### Phase 2: Sentinel Stake Snapshot Cache ✅ COMPLETE

- [x] P5_02_01 Create `sentinel/src/stake_snapshot_cache.rs` — `StakeSnapshotCache` struct
- [x] P5_02_02 Three-tier cache: local `Mutex<HashMap>` → `DataProvider` fallback → peer request (reserved)
- [x] P5_02_03 Public API: `get(epoch)`, `put(epoch, snapshot)`, `current_epoch()`, `cached_count()`
- [x] P5_02_04 Add `parking_lot` + `log` dependencies to sentinel `Cargo.toml`
- [x] P5_02_05 Wire into `Sentinel` constructor — `sentinel/src/sentinel.rs`
- [x] P5_02_06 Export `StakeSnapshotCache` from sentinel crate lib.rs — `sentinel/src/lib.rs`
- [x] P5_02_07 4 tests: cache_empty_returns_none, cache_put_and_get, cache_fallback_to_data_provider, cache_independent_epochs

### Phase 3: Deterministic Finalizer Assignment ✅ COMPLETE

- [x] P5_03_01 Add `SentinelError::Routing(String)` variant — `sentinel/src/sentinel.rs`
- [x] P5_03_02 Implement `assign_finalizer_deterministic()` — uses snapshot + `deterministic_select`
- [x] P5_03_03 Implement `assign_finalizer_deterministic_retry()` — retry suffix if assigned key matches rejected
- [x] P5_03_04 Wire into `handle_process_request` — replace empty `finalizer_public_key: vec![]` with deterministic assignment
- [x] P5_03_05 Wire into `handle_rejection` — replace random `candidates.into_iter().next()` with deterministic + fallback to node registry

### Phase 4: Epoch Number on Block ✅ COMPLETE

- [x] P5_04_01 Add `epoch_number: u64` field to `Block` struct — `src/blocks.rs`
- [x] P5_04_02 Update `Block::from_transaction()` to accept `epoch_number` param — `src/blocks.rs`
- [x] P5_04_03 Update `Block::test_block()` to default `epoch_number: 0` — `src/blocks.rs`
- [x] P5_04_04 Update `Token::create_block()` to accept and propagate `epoch_number` — `src/tokens.rs`
- [x] P5_04_05 Update `BlockBuilder::create_block()` to accept `epoch_number` — `finalizer/src/block_builder.rs`
- [x] P5_04_06 Update all Block struct literals across 4 files (8 locations) — `block_builder.rs`, `message_dispatcher.rs`, `committer.rs`, `validation.rs`

### Phase 5: Epoch Boundary Snapshot Persistence ✅ COMPLETE

- [x] P5_05_01 Wire `save_stake_snapshot` into `Committer::handle_epoch_reconcile` — after reconciliation + leader election
- [x] P5_05_02 Wire `save_stake_snapshot` into `Committer::advance_epoch` — on epoch advance
- [x] P5_05_03 Use `token_partition_id` as DataProvider partition key

### Phase 6: Finalizer Epoch Verification ✅ COMPLETE

- [x] P5_06_01 Add `current_epoch: u64` field to `Finalizer` struct — `finalizer/src/finalizer.rs`
- [x] P5_06_02 Update `Finalizer::new()` to accept `current_epoch` param
- [x] P5_06_03 Wire `self.current_epoch` into `try_finalize` block creation
- [x] P5_06_04 Add `advance_epoch()` and `current_epoch()` accessors
- [x] P5_06_05 Update test constructor with epoch=0

**Total tests: 345 passing (21 committer + 256 core + 9 executor + 26 finalizer + 32 sentinel)**

---

## Phase 5: Refactor pneumatic_committer

### 5.1 Committer

- [x] P5_01 Update `pneumatic_committer/Cargo.toml` — `pneumatic_committer/Cargo.toml` — structural
- [x] P5_02 Refactor `Committer` struct with gossiper, block_services, token_distributor — `pneumatic_committer/src/lib.rs` — refs: C# Committer.cs:32-54
- [x] P5_03 Implement `check_and_commit_transaction_results` — `pneumatic_committer/src/lib.rs` — refs: C# Committer.cs:66-94
- [x] P5_04 Simplify `validate_transaction_message` — `pneumatic_committer/src/lib.rs` — refs: C# Committer.cs:97-103
- [x] P5_05 Use Result throughout (no silent logger.log failures) — `pneumatic_committer/src/lib.rs`

### 5.2 EpochManager

- [x] P5_06 Create `epoch_manager/` directory — `pneumatic_committer/src/epoch_manager/mod.rs` — structural
- [x] P5_07 Implement `CommitterBlockServices` — `pneumatic_committer/src/epoch_manager/committer_block_services.rs` — refs: C# CommitterBlockServices.cs
- [x] P5_08 Implement `StakingManager` with concrete types — `pneumatic_committer/src/epoch_manager/staking_manager.rs` — refs: C# StakingManager.cs
- [x] P5_09 Implement `EpochReconciler` — `pneumatic_committer/src/epoch_manager/epoch_reconciler.rs` — refs: C# EpochReconciler.cs
- [x] P5_10 Implement `LeaderSelector` (stubbed) — `pneumatic_committer/src/epoch_manager/leader_selector.rs`

### 5.3 Main

- [x] P5_11 Update `main.rs` — `pneumatic_committer/src/main.rs` — structural

## Phase 6: Crypto Implementation

- [x] P6_01 Implement `Ed25519Provider` (sign/verify/public_key) via `ed25519-dalek` — `crypto.rs` — refs: C# IAsymmetricalEncryptionProvider.cs, RFC 8032
- [x] P6_02 Implement `BasicHashProvider::hash` using ring/SHA-256 — `crypto.rs` — refs: C# IHashProvider.cs
- [x] P6_03 EnvironmentMetadata crypto provider uses `RwLock` — `environment.rs`
- [x] P6_04 Implement `encrypt`/`decrypt` stubs (hybrid AES-GCM + X25519 key exchange) — `crypto.rs` — uses `aes-gcm` 0.11.0 + `x25519-dalek` 3.0.0; wire format: `[32-byte ephemeral PK][ciphertext + 16-byte GCM tag]`
- [x] P6_05 Implement `encrypt_to`/`decrypt_from` for cross-recipient encryption — `crypto.rs` — extend trait with methods accepting recipient's X25519 public key; shared DH via private `dh_encrypt`/`dh_decrypt` helpers; added `x25519_public_key()` accessor

## Phase 7: Tests (345 passing across 5 crates — 256 core + 32 sentinel + 26 finalizer + 9 executor + 21 committer)

All tests use inline `#[cfg(test)] mod tests` blocks (no external `tests/` directory).
Factory helpers follow `make_*` pattern. Concurrent tests use `std::thread::spawn` with `Arc`-shared DashMaps.

- [x] T01 Add tests for TransactionState transitions — `transactions.rs` — 14 tests: lifecycle, acquire/release, state predicates
- [x] P1_Add tests for PneumaticError variants — `errors.rs` — 10 tests: From impls, risk scoring, validation error matching
- [x] P1_Add tests for PendingTransactionRegistry — `registry.rs` — 22 unit tests (CRUD, state transitions, validation result lookup) + 11 concurrent tests (atomic ops, race safety, stress)
- [x] P1_Add tests for PendingTransaction acquire/release — `registry.rs` — included in registry tests above
- [x] P1_Add tests for Gossiper — `gossiper.rs` — 9 tests: accept first, ignore duplicate, accept different, capacity, fan-out invokes-all, fan-out receives-copy, fan-out dedup-skips-all, fan-out three-handlers, fan-out concurrent-invocation
- [x] P1_Add tests for ValidationSpec — `validation.rs` — 17 tests: SelfSignedBlockValidatorSpec, ExecutedBlockValidatorSpec, ValidationSpecRegistry, nonce validation
- [x] P2_Add tests for Sentinel message routing — `sentinel/src/sentinel.rs` — 16 tests: From impls, creation, spec name routing, action dispatch, self-signed flow, compute_gas_used (3), TransactionNotifier send methods (4)
- [x] P4_Add tests for SignatureCollector quorum logic — `finalizer/src/signature_collector.rs` — 8 tests: add_success, add_duplicate_fails, add_multiple, check_quorum_met, check_quorum_not_met, reconcile_stake_weighted_supermajority, reconcile_single_sets_winner, reconcile_zero_stake_empty, reconcile_all_needed, plus 3 concurrent tests: multi-thread add, duplicate rejection, quorum during concurrent adds
- [x] P4_Add tests for BlockBuilder — `finalizer/src/block_builder.rs` — 2 tests: build_signed_transaction, create_block
- [x] P4_Add tests for MessageDispatcher — `finalizer/src/message_dispatcher.rs` — 2 tests: send_to_committers, send_clear_to_sentinels
- [x] P3_Add tests for Executor — `executor/src/executor.rs` — 5 tests: validation result, backpressure cycle
- [x] T07 Migrate existing tests — all test-bearing files — total 345 tests across 5 crate targets (256 core + 32 sentinel + 26 finalizer + 9 executor + 21 committer) — +22 tests from Protocol Rearchitecture (4 FinalityStatus + 8 CandidateRegistry + 6 Phase 2 conflict detection + 4 Phase 3 conflict wiring) — +18 tests from Phase 5 deterministic routing (5 selection + 4 cache + 2 block_builder + 2 message_dispatcher + 5 other)
- [x] T08 Self-validated token flow end-to-end — `validation.rs` — integration test exercising full self-signed pipeline (token → spec validate → PendingTransaction → Validated → registry lookup)
- [x] T09 Backpressure verification — `executor/src/executor.rs` — `full_backpressure_cycle`: preload at capacity → reject → cleanup → retry succeeds

## Security Audit Remediation (2026-08-11 external audit)

Critical findings from external code audit — ordered by severity + impact. Items 1-4 are **blocking** (render the system non-functional or cryptographically unsafe). Items 5-7 are **high risk** (DoS / correctness hazards). Items 8-9 are **medium risk** (known gaps, tracked in roadmap).

### SA_01 Fix wire framing buffer allocation — `src/conns.rs:37,51`

**Severity:** Critical. ~~The protocol is completely broken for inbound messages.~~ **FIXED (2026-08-11)**

**Bug:** `vec![0u8, 4]` creates a 2-element vector `[0, 4]` instead of a 4-element buffer of zeros. `read_exact` consumes only 2 bytes from the socket. `usize::from_be_bytes` needs 8 bytes on 64-bit — `try_into()` always fails, `unwrap_or_default()` returns `0`. Every inbound frame reads zero-length payload, desyncing the length-prefixed protocol. Sender-side writes 8 bytes (`to_be_bytes()` on `usize`), so reader and writer are fundamentally incompatible.

**Fix applied:**
```rust
// Line 37 (get_data) and line 51 (get_data_async):
let mut header = [0u8; 4];  // was vec![0u8, 4]
reader.read_exact(&mut header)?;
let data_length = u32::from_be_bytes(header) as usize;  // was usize::from_be_bytes(header.try_into().unwrap_or_default())

// Line 99 (TcpConnection::send):
let length_header = (data.len() as u32).to_be_bytes();  // was data.len().to_be_bytes() — wrote 8 bytes on 64-bit
```

**Trait compatibility:** Updated `Stream::read_exact` and `StreamReader::read_exact` from `&mut Vec<u8>` to `&mut [u8]` (standard library convention, works with arrays and vectors).

**Tests added (7 passing):** `tcp_wire_framing_simple` (17-byte TCP), `tcp_wire_framing_large` (1KB TCP), `tcp_wire_framing_zero` (empty payload), `tcp_wire_framing_boundary` (256-byte 0x00000100 boundary), `uds_wire_framing` (TCP/UDS), `tcp_async_wire_framing` (async StreamReader/StreamWriter split), `tcp_wire_framing_round_trip` (stub, channel type conflict).

**Estimate:** 2h — **ACTUAL: ~1h** (fixed and tested)

### SA_02 Make leader election deterministic and verifiable — `src/epoch.rs:154-186`

**Severity:** Critical. ~~Every node picks a different leader per `select()` call — consensus is impossible.~~ **FIXED (2026-08-11)**

**Bugs:** (1) `rand::thread_rng()` was unseeded, non-reproducible, non-deterministic. (2) Iterating `HashMap::iter()` for stake walk had randomized order (SipHash). Two independent non-determinism sources.

**Fix applied:**
```rust
// src/epoch.rs — LeaderSelector::select()
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;

fn select(&self, stakers: &StakeSet, epoch_number: u64) -> Vec<u8> {
    let total = stakers.total_stake();
    if total == 0 { return vec![]; }

    // Deterministic seed: SHA-256(epoch_number.to_be_bytes())
    let digest = ring::digest::digest(&ring::digest::SHA256, &epoch_number.to_be_bytes());
    let mut rng = StdRng::from_seed(digest.as_ref().try_into().unwrap_or_else(|_| {
        unreachable!("SHA-256 always produces 32 bytes")
    }));
    let target: u64 = rng.gen_range(0..total);

    // Deterministic iteration: sorted keys instead of HashMap::iter()
    let mut keys: Vec<&Vec<u8>> = stakers.stakers.keys().collect();
    keys.sort();

    let mut cumulative = 0u64;
    for key in keys {
        let stake = *stakers.stakers.get(key).unwrap();
        cumulative += stake;
        if cumulative >= target { return key.clone(); }
    }
    keys[0].clone() // fallback
}
```

**Call sites updated:** `Epoch::new_with_leader`, `EpochBoundaryDetector::advance_to_new_epoch`, `Committer::handle_epoch_reconcile` (with `AtomicU64` epoch tracking), committer's `LeaderSelector::select_internal`. Trait signature: `select(&self, stakers: &StakeSet, epoch_number: u64) -> Vec<u8>`.

**Design note:** Seed = `SHA-256(epoch_number.to_be_bytes())` via `ring` (no external sha2 dependency). Later upgradable to `SHA-256([epoch_number || prev_block_hash])`. Sort = lexicographic on `Vec<u8>`.

**Test added:** `leader_selector_deterministic_same_inputs_same_output` — asserts 20 identical calls with same inputs produce same leader. `leader_selector_deterministic_different_epochs_can_differ` replaces old random variants test.

**Estimate:** 6h — **ACTUAL: ~2h**

### SA_03 Hardcoded nonce check always validates against 0 — `src/action_router.rs`

**Severity:** Critical. ~~`check_nonce(&sender, 0)` only ever succeeds for the sender's first transaction.~~ **FIXED (2026-08-11)**

**Bug:** `check_nonce(&sender, 0)` always passed hardcoded `0`. The `message.body` contained a MsgPack-serialized `Transaction` with `sequence_number` (nonce) and `amount` fields — both were ignored. "Process" and "Preload" handlers also hardcoded `amount=0` for gas calculation.

**Fix applied:**
```rust
// src/action_router.rs — Process handler
use crate::transactions::Transaction;
use crate::encoding::deserialize_rmp_to;

"Process" => {
    let tx: Transaction = deserialize_rmp_to(&message.body)
        .map_err(PneumaticError::from)?;
    let nonce_result = self.check_nonce(&sender, tx.sequence_number).await?;
    let gas_result = self.verify_gas(&sender, "Process", tx.amount.unwrap_or(0)).await?;
    // ...
}

// Preload handler: same pattern, extracts amount for verify_gas
```

**Design note:** Nonce comes from `message.body` (cryptographically bound by sender's signature), NOT top-level `Message` struct (signature only covers body, not metadata fields).

**Tests added:** `route_process_invalid_body_returns_encoding_error` — corrupt body → Encoding error. `route_process_nonce_mismatch_from_body_returns_invalid_nonce` — body seq=5, user nonce=3 → InvalidNonce. 2 existing tests renamed, 4 existing tests updated with serialized body helpers.

**Estimate:** 2h — **ACTUAL: ~1h**

### SA_04 Raw X25519 DH output as AES key + always-zero nonce — `src/crypto.rs`

**Severity:** Critical. AES-GCM nonce reuse under the same key enables full plaintext recovery and forgery. Without a KDF between DH output and symmetric key, the entire scheme's safety rests on one unenforced invariant (fresh ephemeral scalar every call).

**FIXED (2026-08-11)**

**Applied fixes:**

1. **HKDF key derivation**: Added `hkdf = "0.12"` + `sha2 = "0.10"` deps. New `derive_aes_key()` converts X25519 shared secret through HKDF-SHA256 with info `b"aes256-gcm-key"` → clean 256-bit AES key.

2. **Random nonce**: New `generate_nonce()` uses `getrandom` for 12 random bytes per encryption. Replaces `Nonce::default()` (all zeros).

3. **Wire format**: Changed from `[32-byte ephemeral PK][ciphertext + 16-byte GCM tag]` → `[32-byte ephemeral PK][12-byte nonce][ciphertext + 16-byte GCM tag]` (+12 bytes).

4. **Deprecated API**: Migrated `Nonce::from_slice` → `Nonce::try_from(slice)` (aes-gcm 0.11.0 deprecation).

**Tests added:** `test_different_nonces_per_encryption` — same plaintext encrypted twice produces different ciphertexts. `test_encrypt_to_different_ciphertexts` — two senders encrypting to same recipient produce different ciphertexts.

**Wire size change:** empty data 48 → 60 bytes.

**Estimate:** 4h → actual ~2h

---

### SA_05 Replace panics with error returns on network-reachable paths — `src/crypto.rs`, `src/environment.rs` ✅ COMPLETE

**Severity:** High. A single malformed/tampered message can panic a handling thread → remote DoS.

**Locations:**
- `crypto.rs`: `.expect("AES-GCM decryption failed...")` on decryption failure
- `crypto.rs`: `panic!("Decrypt input too short...")` on payload length check
- Throughout: `RwLock::read().expect("poisoned")` / `write().expect("poisoned")`

**Fix:** Return `Result<T, PneumaticError>` instead of panicking:
```rust
// Instead of:
self.crypto_provider.read().expect("RwLock poisoned").decrypt(...)

// Use:
self.crypto_provider.read().map_err(|e| PneumaticError::CryptoError(format!("RwLock poisoned: {:?}", e)))?
    .decrypt(...)
    .map_err(|e| PneumaticError::CryptoError(e.to_string()))?
```

Added `PneumaticError::CryptoError(String)`, `ConnError::DecryptError(String)`, and `DataError::CryptoError(String)` variants. RwLock poisoning and GCM decryption errors now return errors gracefully instead of panicking.

**Also fixed:** TOCTOU race in `PendingTransactionRegistry::add_transaction` (concurrent inserts of same id could both succeed under `contains_key`-then-`insert`); replaced with atomic `insert`-return-check.

**Tests added:** `test_decrypt_short_input_returns_error`, `test_decrypt_from_wrong_recipient_returns_error`, `pneumatic_error_display_crypto_error`, `pneumatic_error_from_conn_error_decrypt`, `data_error_crypto_error_display`, `data_op_display`, `get_op_display`, `save_op_display`, `block_validation_error_display`.

**Verification:** `cargo test --workspace` — 299 passing, 1 ignored.

**Estimate:** 4h → actual ~1h

### ~~SA_06~~ Deterministic gas accounting — integer math only — `src/action_router.rs` ✅ COMPLETE

**Severity:** High. `f64` arithmetic is not bitwise-identical across CPU architectures. If gas computation feeds into committed state, nodes diverge.

**Changes:**
- `src/environment.rs`: Added `CostModel::compute_gas(amount, multiplier)` using integer fixed-point math (scale 10_000). `multiplier_to_fixed()` converts f64 to integer; `gas_from_amount()` uses `saturating_mul`/`saturating_add` for overflow safety.
- `src/action_router.rs`: `verify_gas` now calls `CostModel::compute_gas` instead of `amount as f64 * multiplier`.
- `sentinel/src/transaction_validator.rs`: `compute_gas_used` refactored to use shared `CostModel::compute_gas`, eliminating duplicated f64 logic.
- Added 4 tests: `compute_gas_deterministic_integer_math`, `compute_gas_preload_multiplier`, `compute_gas_zero_amount_base_cost`, `compute_gas_fractional_multiplier`.

**Verification:** `cargo test --workspace` — all 244 tests passing (core: 237, sentinel: 25, committer: 22, executor: 10, finalizer: 9), 1 ignored.

**Why this matters:** Integer fixed-point gas arithmetic is bitwise-identical across all CPU architectures, eliminating non-determinism from FPU precision differences. Saturating operations prevent overflow panics from malformed amounts.

**Estimate:** 2h → actual ~30min

### ~~SA_07~~ Rename misleading `AsymCryptoProviderType::RSA` → `Ed25519` — `src/crypto.rs`

**Completed:** 2026-08-11 — Renamed enum variant in `crypto.rs`, updated match arm, cleaned comment, updated 4 test JSON configs.

**Severity:** Medium (currently harmless, future landmine). The enum variant was named `RSA` but instantiates `Ed25519Provider`.

---

### ~~SA_08~~ Add max frame size limit — `src/conns.rs` ✅ COMPLETE

**Completed:** 2026-08-11 — Added `MAX_FRAME_SIZE` (16 MB) constant, bounds checks in `get_data()` and `get_data_async()` returning `ConnError::MalformedData` on violation. Two integration tests verify oversized rejection and valid acceptance.

**Severity:** Medium. Once the framing bug (SA_01) is fixed, `vec![0u8; data_length]` allocates based on an attacker-controlled 8-byte length with no upper bound → trivial memory-exhaustion DoS.

### SA_09 TLS for transport security — tracked in roadmap Phase 7

**Severity:** Medium. Plain TCP/Unix sockets with no encryption or authentication. Fine for testnet/localhost, but a prerequisite for anything beyond a trusted environment.

**Current status:** Already tracked in README.md Phase 7 under "Networking". No action needed beyond tracking.

---

## Stubbed Functionality Inventory

Items marked **DONE** have been implemented (see commit notes). Remaining items are active stubs/TODOs.

### pneumatic_core — Priority 1 (Core runtime functions)

#### Crypto provider — `encrypt`/`decrypt` now implemented (P6_04) (sign/verify via ed25519-dalek)
**File:** `src/crypto.rs:74-123`
Hybrid AES-256-GCM + X25519 key exchange. Each `encrypt()` generates a fresh ephemeral keypair, derives shared secret via DH, encrypts payload. Wire format: `[32-byte ephemeral PK][ciphertext + 16-byte GCM tag]`.
**Dependencies:** `aes-gcm` 0.11.0, `x25519-dalek` 3.0.0 (with `static_secrets` + `getrandom` features).
**Cross-recipient:** `encrypt_to`/`decrypt_from` (P6_05) extends encryption to arbitrary recipients via their X25519 public key.

#### ActionRouter — routing + coordination now fully implemented (P1_44)
**File:** `src/action_router.rs`
All action branches dispatch and all coordination helpers use protocol-level users: `check_nonce()` calls `get_user()` from data store, `verify_gas()` calculates `base_cost + (amount × multiplier)` from `CostModel` and returns usage tracking, `check_stake()` verifies both `cost_model.global_min_stake` AND `config.get_min_type_stake()`. Fails with `InvalidNonce`, `InsufficientGas`, or `InsufficientStake`. Returns `GasVerified { gas_used, gas_remaining }` and `StakeChecked { node_type, stake }`. 345 tests across workspace (256 core + 32 sentinel + 26 finalizer + 9 executor + 21 committer).

#### Protocol-level User + gas model — FOUNDATION COMPLETE (P1_44 updated)
**File:** `src/user.rs`, `src/tokens.rs`, `src/data.rs`, `src/environment.rs`, `src/action_router.rs`, `src/node/registry.rs`
**Completed:** `User` struct has `stake` field (separate from `fuel_balance`). `Account` struct added for per-token balances. `CostModel` added to `EnvironmentMetadata` with `base_cost`, `global_min_stake`, `admin_public_key`, `admin_tax_percentage`, `amount_multiplier`. `DataProvider` trait has `get_user()`/`save_user()` methods. `DefaultDataProvider` and `StubDataProvider` implement them. `ActionRouter::check_nonce()`, `verify_gas()`, `check_stake()` use `get_user()` instead of `get_token() + get_asset::<User>()`. `verify_gas()` calculates real gas cost with per-action multipliers. `check_stake()` checks both `cost_model.global_min_stake` AND `config.get_min_type_stake()`. `node/registry::check_db_node_user()` updated. 311 tests passing.

#### TokenFactory — minting fee deduction — DONE
**File:** `src/tokens.rs:290-347`
`TokenFactory::mint_token_full()` calculates `minting_fee = base_cost * 10`, deducts from owner's `fuel_balance` via `data_provider`, records `admin_tax = fee * admin_tax_percentage` as `PendingAdminCredit` in `admin_credit_registry`. Returns `InsufficientGas` when balance < fee. 7 tests: `mint_token_full_deducts_fee_from_balance`, `mint_token_full_records_admin_tax_credit`, `mint_token_full_no_admin_tax_when_zero_percentage`, `mint_token_full_insufficient_gas_error`, `mint_token_full_zero_base_cost_no_deduction`, `mint_token_full_admin_credit_taken`.
**Note:** `mint_token()` (backward-compat) and `mint_user_token()` remain free — `mint_token_full` is the charged variant.

#### ActionRouter — per-action gas cost from CostModel — DONE
**File:** `src/environment.rs:26`, `src/action_router.rs`
`CostModel` has `amount_multiplier: HashMap<String, f64>` with defaults `{"Process": 1.0, "Preload": 2.0, "Sign": 1.5}`. `verify_gas()` computes `gas_used = base_cost + (amount × multiplier_for_action)` and returns `GasVerified { gas_used, gas_remaining }`. Default multipliers set in `CostModel::default_amount_multiplier()`.

#### Gas deduction after executor completes — DONE
**File:** `src/registry.rs` (gas_tracker), `sentinel/src/transaction_validator.rs` (compute_gas_used), `sentinel/src/sentinel.rs` (record_gas_used), `committer/src/committer.rs` (gas deduction in check_and_commit_transaction_results)
**Completed:** `PendingTransactionRegistry` has `gas_tracker: Mutex<HashMap<String, u64>>` with `record_gas_used()`/`get_gas_used()` methods. `TransactionValidator::compute_gas_used()` computes `gas_used = base_cost + (amount × multiplier)`. Sentinel calls `record_gas_used` during validation (both received and self-signed paths). Committer's `check_and_commit_transaction_results` deducts `gas_used` from sender's `fuel_balance` via `saturating_sub` after block commit. `Committer` has `data_provider` field injected. 292 tests passing across workspace.

#### Transaction ordering — race conditions across senders
**File:** `src/epoch.rs` (LeaderSelector) → `PendingTransactionRegistry` → `ActionRouter` pre-flight
**Current state:** Foundation complete. `TransactionPool` provides per-token deterministic ordering (sorted by sender ASC, sequence_number ASC, timestamp ASC). `LeaderSelector` implements stake-weighted random selection. `BlockProposer` dequeues and wraps transactions in `SignedTransaction` with leader metadata. `EpochBoundaryDetector` detects stale blocks and advances epochs. `resolve_block_conflict()` handles conflicting block proposals with stake-based resolution and hash tie-break. `PendingTransactionRegistry` has `transition_to_validated_and_enqueue()`, `enqueue_to_pool()`, `dequeue_for_leader()`, `get_ordered_transactions()`. Error variants: `PneumaticError::StaleBlock`, `PneumaticError::BlockConflict`, `ValidationFailureReason::StaleEpochBlock`, `ValidationFailureReason::BlockConflict`.
**Completed (2026-08-11):** Pipeline fully wired. Sentinel's `handle_self_signed()` and `handle_process_request()` both call `transition_to_validated_and_enqueue()` to populate the TransactionPool. Committer's `propose_blocks()` checks epoch leader, detects expiry, dequeues from pool via `BlockProposer`, builds `TransactionCommit`. Background epoch loop spawned in `main.rs` polling every 5 seconds. Epoch components (Epoch, EpochBoundaryDetector, BlockProposer) wired in `main.rs`. 9 new tests (3 sentinel + 6 committer). Total: 345 tests passing across 5 crates.
**Completed (2026-08-12):** Finalizer → Committer commit message path wired. `check_and_commit_transaction_results()` now accepts transactions in both `Finalizing` (standard pipeline) and `Validated` (leader-proposal) state, transitions to `Committed`, and releases the lock. 2 new tests for leader-proposal commit path with gas deduction and overflow saturation.
**Completed (2026-08-12):** SignatureCollector `reconcile_signatures()` now implements stake-weighted supermajority selection. Sorts candidates by stake descending, accumulates until quorum threshold reached, returns winning set with `conflict_resolved=true`. 4 new tests.
**Remaining:** `Finalizer.initialize` — gossiper message handler subscription; `Finalizer.try_finalize` — placeholder data for total_stake, total_voters, previous_hash.

#### Gossiper — handler stored and wired ✓ (DONE)
**File:** `src/gossiper.rs:23`
Handler stored as `Mutex<Option<Box<dyn Fn(Vec<u8>) + Send + Sync>>>`. The `initialize()` method stores the closure and `handle_message()` invokes it after dedup check.
**Wiring:** Sentinel: `sentinel.initialize(move |raw| { if let Err(e) = arc.on_data_received(raw) { ... } })`. Committer: wraps caller's `Fn(Message)` with deserialization. Finalizer: stub (no Gossiper field yet).

#### Gossiper — no crypto validation — done (crypto_provider injected, check_signature called after dedup)
**File:** `src/gossiper.rs:84-92`
`handle_message()` now validates `AsymCryptoProvider.check_signature(message.signature, message.public_key, message.body)` after dedup and before fan-out. Invalid messages return `DataError::InvalidSignature`.

#### Gossiper — fan-out to handler delegates done (P1_42)
**File:** `src/gossiper.rs:87-92`
Handlers stored as `Vec` behind `Mutex`; `initialize()` registers first, `add_handler()` registers extras; `handle_message()` clones raw_data and invokes each sequentially.

#### EpochReconciler — DONE (Phase 2 replaces with same-chain detection, 2026-08-12)
**File:** `committer/src/epoch_manager.rs`, `committer/src/committer.rs`
Phase 2: `reconcile_internal()` now uses `CandidateRegistry` for same-chain fork detection. Accepts `Arc<StakeStore>` + `Arc<CandidateRegistry>` + `Arc<dyn DataProvider>` + env_id + token_ids. Builds `StakeSet` from `StakeStore`, checks `candidate_count(token_id, tip_hash) >= 2` per token, reports pairwise conflicts with real `stake_a`/`stake_b` from `StakeStore::get_stake()`. Misshapen chain detection preserved. Phase 5 (cross-token) removed. 6 new tests.

#### StakingManager — DONE (StubStakingManager superseded)
**File:** `src/epoch.rs:136-142` (stub in core), `committer/src/epoch_manager.rs:78-133` (real impl)
`StubStakingManager::apply_ops()` in core returns `Ok(())` — no-op. Replaced by `StakingManager` in `committer` which applies all ops (AddStaker, RemoveStaker, Slash, Reward) to `StakeStore` via DashMap-backed concurrent storage.

#### LeaderSelector — stake-weighted deterministic selection ✓ (DONE — SA_02, 2026-08-11)
**File:** `src/epoch.rs:154-186`, `committer/src/epoch_manager.rs:220-263`
Replaced `StubLeaderSelector` with `LeaderSelector` using cumulative stake range approach. Deterministic seed: `SHA-256(epoch_number.to_be_bytes())` via `ring`, produces `StdRng` — every honest node with same `StakeSet` + `epoch_number` picks same leader. Replaced `HashMap::iter()` with sorted key walk. Trait: `select(&self, stakers: &StakeSet, epoch_number: u64) -> Vec<u8>`. Also added `IBlockProposer` trait with `BlockProposer` implementation, `EpochBoundaryDetector` struct, and `resolve_block_conflict()` free function. New dependency: `rand = "0.8"`.
**Tests:** 23 new tests (9 LeaderSelector/Epoch, 5 BlockProposer, 9 EpochBoundaryDetector/conflict resolution) + determinism regression test. 256 core + 32 sentinel + 26 finalizer + 9 executor + 21 committer = 345 total.

#### Conflict Resolution — wired into commit path ✓ (DONE — 2026-08-13)
**File:** `src/epoch.rs:393-430` (ConflictResolution enum + enriched resolve_block_conflict), `committer/src/committer.rs:380-487` (handle_conflict_at_commit), `committer/src/committer.rs:78-84` (CandidateRegistry field)
`resolve_block_conflict()` now returns `ConflictResolution` enum: `DiscardLoser` (different proposers, network race), `SameProposerSlash` (same proposer double-signed), `TieFlagBoth` (equal stakes, hash tie-break). The Committer's `handle_conflict_at_commit()` checks `CandidateRegistry` at commit time before `commit_block()`. On conflict, resolves with real stakes from `StakeStore`. `SameProposerSlash` emits `StakingOp::Slash` via `StakingManager.apply_ops()`.
**Tests:** 4 new committer tests (no conflict → inserts first candidate, conflict + different stakes → DiscardLoser, conflict + same proposer → SameProposerSlash, no existing candidates → inserts first). Plus 1 new core test (conflict_resolution_same_proposer_returns_slash). Total: 345 tests.

#### Registry — finalizer_public_key propagation — DONE
**File:** `src/registry.rs:131-132`
`set_requested_finalizer()` calls `transition_to_finalizing(transaction, finalizer_key)` which stores the key in `Finalizing { finalizer_key }`. `Finalizer::try_finalize()` reads it from `Finalizing` state — works correctly. The commented-out `validation.finalizer_public_key = ...` is intentional; the key is stored in `Finalizing` state, not duplicated in `TransactionValidationResult`.

#### Token.get_asset_mut — DONE
**File:** `src/tokens.rs:114-147`
Replaced with three methods: `asset_mut(&mut self) -> Option<&mut Vec<u8>>` returns direct mutable access to raw serialized bytes; `set_asset(&mut self, &impl Serialize) -> Result<(), Error>` serializes and stores; `update_asset<T, F>(&mut self, F: FnOnce(&mut T)) -> Option<T>` deserializes, calls closure to mutate, re-serializes. 5 new tests.

#### EnvironmentMetadataSpec — unused fields — DONE
**File:** `src/environment.rs:145-165` (spec), `environment.rs:17-84` (metadata)
All 5 fields now wired in `load_from_spec`:
- `sym_crypto_provider` and `serialization_provider` stored as `String` fields on `EnvironmentMetadata` for diagnostics
- `trans_validation_specs` and `block_validation_specs` iterated and registered into existing `ValidationSpecRegistry` / `BlockValidatorSpecRegistry` by name ("SelfSigned", "Executed"); unknown names silently skipped
- `allowed_token_types` already stored (line 132); no enforcement needed
Added imports for `SelfSignedBlockValidatorSpec` and `ExecutedBlockValidatorSpec`. 311 tests pass.

#### Config — node registry type selection — DONE
**File:** `src/config.rs:37-46, 126-162`
Implemented three deferred todos: (1) `default_node_registry_types()` selects all 5 types for full nodes, core 4 (minus Archiver) for light nodes; (2) `default_type_configs()` populates per-type `NodeTypeConfig` with min=1, max=1000, min_stake=10 for all registry types; (3) `default_min_stake()` shared constant (10). `get_min_type_stake()` falls back to `default_min_stake()` instead of `u64::MAX` for unknown types. Added `strum::IntoEnumIterator` import.

#### Server worker — exits after one job — DONE
**File:** `src/server.rs:116-147`
Both `get_sync_thread` and `get_async_thread` loop body fixed: changed `Err` branches from `return Err(WorkerError::WhileReceiving(...))` to `return Ok(())` — when the channel closes the loop exits cleanly. Added explicit `continue` in sync thread after `job()` for clarity. Previously the `return` after job processing caused threads to exit prematurely.

#### Server async poison test — hangs — DONE
**File:** `src/server.rs:252-275`
Test was commented out because `thread::spawn` with async blocks in a Tokio context causes hangs. Fixed by updating the server loop (above) — workers now exit cleanly on channel close. The underlying issue was the same: workers not continuing to process jobs. The commented-out test remains deferred until the thread pool is restructured to use a proper tokio runtime for async jobs.

#### TcpConnection — cleanup on drop — DONE
**File:** `src/conns.rs:69-117`
Added `Drop` impl that drops `writer` first (closing the write half of the split stream, signaling the reader to stop), then drops `listening_thread` via `take()` to detach the OS thread. Wrapped both fields in `Option` for safe removal. Removed the `// TODO: initiate drop` comment.

#### SA_01 Wire framing buffer allocation — DONE
**File:** `src/conns.rs:37,51,99` + `src/conns/streams.rs`
Fixed 3 bugs: `vec![0u8, 4]` → `[0u8; 4]` (header buffer), `usize::from_be_bytes(...try_into()...)` → `u32::from_be_bytes(header) as usize` (correct decoding), `(data.len() as u32).to_be_bytes()` (4-byte header, was 8 on 64-bit). Updated `Stream::read_exact` and `StreamReader::read_exact` signatures from `&mut Vec<u8>` to `&mut [u8]`. Added 7 integration tests exercising real TCP/UDS socket paths with length-prefixed framing.

#### NodeRegistry.send_to_all — uses registered connections — DONE
**File:** `src/node/registry.rs:163-202`
Rewrote `send_to_all` to use registered `Connection` objects from `NodeRegistryNode.conn` via `conn.send(&data).await`. Uses `futures::future::join_all` for concurrent broadcasts. Added `send_to_all_blocking` for sync contexts. Changed `Connection::send` to take `&self` with `tokio::sync::Mutex` interior mutability inside `TcpConnection`. `futures = "0.3"` dependency added.

#### NodeRegistry.process_registration — DONE
**File:** `src/node/registry.rs:204-269`
Implemented `process_registration(RegistrationBatch)` with full batch processing:
- **Add entries**: validates each entry via `check_db_node_user` (DB stake check) and `type_is_maxed_out` (registry limit check). Skips invalid entries. On any rejection, returns `Failure` with details. Inserts valid entries into per-type DashMap with a `NoOpConnection` placeholder (real connections established via `process_conn_request`).
- **Remove entries**: removes by key from the appropriate DashMap for each requested node type.
Added `NoOpConnection` placeholder impl for registrations without live connections.

#### CandidateRegistry — DONE (Phase 1, 2026-08-12)
**File:** `src/epoch.rs` (implemented)
DashMap-backed registry keyed by `(token_id, previous_hash)` holding competing block proposals. Phase 2 consumes it in `EpochReconciler::reconcile_internal()` for same-chain fork detection. 8 tests (6 unit + 2 concurrent).

#### Block finality_status — DONE (Phase 1, 2026-08-12)
**File:** `src/blocks.rs` (implemented)
`FinalityStatus` enum (`Optimistic`, `Confirmed`) on `Block`. Blocks created via `from_transaction`, `test_block`, `create_block` default to `Optimistic`. Serialization round-trip tests pass. 4 tests.

#### Proposer key on Block/SignedTransaction — DONE (Phase 1, 2026-08-12)
**File:** `src/transactions.rs` (SignedTransaction.proposer_key), `src/blocks.rs` (Block.proposer_key)
Explicit proposer public key for conflict-resolution stake lookup. Propagated from leader_address in BlockProposer and BlockBuilder.

#### Vote/Dispute messages — NOT YET IMPLEMENTED (Phase 5)
**File:** `src/messages.rs` (target)
New `Message` variants for conflict voting: "I saw candidate block" and "I vote for block X in this conflict."

#### Conflict-vote aggregation — NOT YET IMPLEMENTED (Phase 5)
**File:** `finalizer/src/` (target)
Structurally similar to `SignatureCollector` but scoped to conflicts rather than per-transaction quorum.

### pneumatic_sentinel — Priority 2 (Node-specific logic)

#### Sentinel.initialize — DONE (gossiper handler wired)
**File:** `sentinel/src/sentinel.rs:78-80`
`.initialize()` accepts a closure and passes it to `self.gossiper.initialize(gossiper_handle)`. Caller creates closure wrapping `self.on_data_received(raw)`.

#### Sentinel.send_to_executor_for_preload — empty stub
**File:** `sentinel/src/sentinel.rs:166-171`
**Action:** Call `self.transaction_notifier.send_to_executors_for_preload(tx, self.env_data)`.

#### Sentinel.handle_confirmation — DONE
**File:** `sentinel/src/sentinel.rs:187-224`
**Completed:** Deserializes tx_id from message body, acquires transaction lock, verifies sender's `public_key` matches assigned finalizer via `registry.is_requested_finalizer()`, extracts `Finalizing` state via `std::mem::replace` and transitions to `Committed` (empty block_hash placeholder), notifies all sentinels via `notify_delete`, releases lock. Added 3 tests: valid finalizer → Committed state, unassigned finalizer → `Registry` error, non-Finalizing state → `Registry` error.

#### Sentinel.handle_rejection — DONE
**File:** `sentinel/src/sentinel.rs:232-316`
**Completed:** Deserializes tx_id, acquires lock, verifies rejecting `public_key` matches assigned finalizer, collects candidate finalizers excluding the rejected one (iterates DashMap), reassigns via `std::mem::replace` + `transition_to_finalizing`, sends `request_single_finalizer` to the new finalizer via targeted send, notifies sentinels via `notify_delete`, releases lock. Added `NoTarget(NodeRegistryType)` variant to `SentinelError`. Added `TransactionNotifier.request_single_finalizer` for targeted finalizer delivery. Added 3 tests: no alternative finalizer → error, unassigned finalizer → error, terminal state → error.

#### Sentinel.handle_register_request — DONE
**File:** `sentinel/src/sentinel.rs:318-357`
**Completed:** Deserializes `NodeRegistryRequest` from message body, rejects if already registered, validates stake via `DataProvider.get_user()` against minimum for requested type, adds node to each requested type's DashMap with `NoOpConnection` placeholder. Added `data_provider: Arc<dyn DataProvider>` field to `Sentinel`, updated `new()` constructor. Added `NodeRegistry.get_config()` accessor. Added 3 tests: sufficient stake → success + verified in registry, already registered → error, insufficient stake → error.

#### TransactionValidator.validate_transaction — spec now invoked — done
**File:** `sentinel/src/transaction_validator.rs:33-70`
`TransactionValidator` now receives `Arc<dyn DataProvider>`. `validate_transaction()` loads the token via `data_provider.get_token()` and delegates to `spec.validate(tx, token, env_data)`. Added `TokenNotFound` variant to `ValidationFailureReason`. `Sentinel::new()` creates `DefaultDataProvider` and passes it through.

#### TransactionNotifier.send_to_nodes — DONE
**File:** `sentinel/src/transaction_notifier.rs`
**Completed:** Injected `Arc<NodeRegistry>` into `TransactionNotifier`. `send_to_nodes` spawns a bare OS thread that creates its own mini Tokio runtime to drive the async `registry.send_to_all()`. Works with or without an existing reactor. All 5 methods (`send_to_executors_for_preload`, `send_to_finalizer_for_preload`, `notify_clear_to_process`, `notify_delete`, `request_finalizer`) now use real networking. Added `From<NotifyError> for SentinelError` impl. Sentinel's `send_to_executor_for_preload` now calls `self.transaction_notifier.send_to_executors_for_preload()` instead of being a no-op. 4 new tests.

### pneumatic_executor — Priority 3 (Started — Phase 3 complete, ~560 lines, 6 tests)

#### Executor — stub contract execution
**File:** `executor/src/executor.rs:298-304`
`execute_contract()` serializes the transaction as "execution output" instead of invoking contract bytecode.
**Action:** Replace stub with actual contract execution logic.

#### Executor — stub finalizer networking
**File:** `executor/src/executor.rs:141-162`
`send_to_finalizer()` broadcasts `Message(action="Execute")` with serialized body, but doesn't build proper execution result or compute result hash.
**Action:** Build proper execution result, hash with `hash_provider`, include in message.

#### Executor — `validate_execution_result` never called
**File:** `executor/src/executor.rs:184-193`
Checks `result_hash` non-empty but never invoked in the pipeline (execution task calls `preload_cleanup` directly).
**Action:** Call after `execute_contract`, use result to transition to Finalizing state.

#### Executor — `get_finalizer_key` never called
**File:** `executor/src/executor.rs:203-208`
Returns finalizer key from registry but never used.
**Action:** Use to send execution result to the correct finalizer.

### pneumatic_finalizer — Priority 4 (Complete: 26 tests pass)

Stubbed within implemented methods:
- `SignatureCollector.reconcile_signatures` — Now fully implemented: sorts candidates by stake descending, accumulates until quorum threshold (2/3) reached, returns winning supermajority set. Sets `winning_finalizer` to the executor that crossed quorum. `conflict_resolved` = true when quorum-crossing signature found. 4 new tests added.
- `Finalizer.initialize` — Message handler subscription via gossiper stubbed (closure parameter accepted but not wired)
- `Finalizer.try_finalize` — Steps 5, 7 use placeholder data (total_stake=0, total_voters=0, previous_hash=[])
- `MessageDispatcher.send_to_all` — Uses NodeRegistry stub, not registered connections (see node/registry.rs:165)

### pneumatic_committer — Priority 5 (Complete: compiles, Phase 5 tasks done)

#### Committer crate — stub comment removed
**File:** `pneumatic_committer/src/lib.rs`
**Done:** Phase 5 tasks (P5_01–P5_11) complete — module declarations, `Committer` struct, `BlockServices`, `epoch_manager` single-file module with `StakeStore`, `StakingManager`, `EpochReconciler`, `LeaderSelector`. Gas deduction wired (committer deducts `gas_used` from sender's `fuel_balance` on commit).

### Tests — Priority 6 (345 passing across 5 crates)

#### Tests added to pneumatic_core — 216 tests
**Files:** `errors.rs` (10), `transactions.rs` (14), `registry.rs` (33 incl. 11 concurrent + 2 gas_tracker), `gossiper.rs` (9), `validation.rs` (17 + 1 integration), `tokens.rs` (7 mint_token_full fee tests), `epoch.rs` (22 LeaderSelector/Epoch/BlockProposer/conflict resolution), `config.rs` (test helpers), `data.rs` (StubDataProvider), `action_router.rs` (18), `crypto.rs` (hash, sign/verify, encrypt/decrypt, cross-recipient), `blocks.rs` (pre-existing), `messages.rs` (pre-existing), `conns.rs` (7 SA_01 wire framing integration tests), `streams.rs` (9 streams unit tests)
**Covered:** PneumaticError variants, TransactionRiskFactor scoring, TransactionState transitions, PendingTransactionRegistry CRUD + concurrent ops + gas_tracker, Gossiper fan-out + dedup, SelfSigned/Executed validation specs, nonce validation, block validation result variants, self-signed token flow integration, TokenFactory minting fee deduction, LeaderSelector stake-weighted selection, BlockProposer, EpochBoundaryDetector, conflict resolution, deterministic_select per-transaction routing (5 tests), epoch_number on Block propagation.

#### Tests added to pneumatic_finalizer — 22 tests
**Files:** `signature_collector.rs` (12 incl. 3 concurrent), `block_builder.rs` (2), `message_dispatcher.rs` (2), plus pre-existing
**Covered:** Signature add/remove, quorum detection, conflict reconciliation, concurrent safety, block building, message dispatch, shutdown behavior.

#### Tests added to pneumatic_sentinel — 25 tests
**Files:** `sentinel/src/sentinel.rs`
**Covered:** SentinelError From impls, construction, spec name routing, action dispatch (process, register, clear), self-signed token flow, compute_gas_used (3: zero amount, preload multiplier, unknown action default), TransactionNotifier send methods (4: executors, finalizer, notify_clear, notify_delete — all verify no-panic with no runtime), handle_confirmation (3: valid finalizer → Committed, unassigned finalizer → error, non-Finalizing state → error), handle_rejection (3: no alternative finalizer → error, unassigned finalizer → error, terminal state → error), handle_register_request (3: sufficient stake → success with registry verification, already registered → error, insufficient stake → error).

#### Tests added to pneumatic_executor — 9 tests
**Files:** `executor/src/executor.rs`
**Covered:** Execution result validation, capacity checks, full backpressure cycle.

#### Tests in pneumatic_committer — 10 tests (7 inline + 3 doc, 3 ignored)
**Files:** `committer/src/epoch_manager.rs` (7 inline: StakeStore concurrent add, StakingManager ops, EpochReconciler conflict detection; 3 doc tests in committer.rs)
**Covered:** Gas deduction in check_and_commit_transaction_results (deducts, no gas tracked, saturates on overflow).

#### Remaining test gaps
**Files:** `config.rs` (unit tests), `data.rs` (DefaultDataProvider tests), `server.rs` (async poison test fix), `epoch.rs` (StubEpochReconciler/StubStakingManager unit tests), `node/registry.rs` (process_registration, send_to_all)
**Action:** Tests for Config::new_for_testing, DefaultDataProvider (TCP/UDS communication), ThreadPool async poison, epoch reconciliation stubs, node registration batch processing.
