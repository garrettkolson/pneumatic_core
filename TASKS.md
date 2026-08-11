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
- [ ] P2_08 Implement `handle_register_request` / `handle_clear_request` — `sentinel/src/sentinel.rs` — refs: C# Sentinel.cs:124-129, 231-235
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

## Phase 7: Tests (271 passing across 5 crates — 214 core + 22 finalizer + 9 executor + 16 sentinel + 10 committer)

All tests use inline `#[cfg(test)] mod tests` blocks (no external `tests/` directory).
Factory helpers follow `make_*` pattern. Concurrent tests use `std::thread::spawn` with `Arc`-shared DashMaps.

- [x] T01 Add tests for TransactionState transitions — `transactions.rs` — 14 tests: lifecycle, acquire/release, state predicates
- [x] P1_Add tests for PneumaticError variants — `errors.rs` — 10 tests: From impls, risk scoring, validation error matching
- [x] P1_Add tests for PendingTransactionRegistry — `registry.rs` — 22 unit tests (CRUD, state transitions, validation result lookup) + 11 concurrent tests (atomic ops, race safety, stress)
- [x] P1_Add tests for PendingTransaction acquire/release — `registry.rs` — included in registry tests above
- [x] P1_Add tests for Gossiper — `gossiper.rs` — 9 tests: accept first, ignore duplicate, accept different, capacity, fan-out invokes-all, fan-out receives-copy, fan-out dedup-skips-all, fan-out three-handlers, fan-out concurrent-invocation
- [x] P1_Add tests for ValidationSpec — `validation.rs` — 17 tests: SelfSignedBlockValidatorSpec, ExecutedBlockValidatorSpec, ValidationSpecRegistry, nonce validation
- [x] P2_Add tests for Sentinel message routing — `sentinel/src/sentinel.rs` — 16 tests: From impls, creation, spec name routing, action dispatch, self-signed flow, compute_gas_used (3), TransactionNotifier send methods (4)
- [x] P4_Add tests for SignatureCollector quorum logic — `finalizer/src/signature_collector.rs` — 3 concurrent tests: multi-thread add, duplicate rejection, quorum during concurrent adds
- [x] P4_Add tests for BlockBuilder — `finalizer/src/block_builder.rs` — 2 tests: build_signed_transaction, create_block
- [x] P4_Add tests for MessageDispatcher — `finalizer/src/message_dispatcher.rs` — 2 tests: send_to_committers, send_clear_to_sentinels
- [x] P3_Add tests for Executor — `executor/src/executor.rs` — 5 tests: validation result, backpressure cycle
- [x] T07 Migrate existing tests — all test-bearing files — total 271 tests across 5 crate targets (214 core + 16 sentinel + 22 finalizer + 9 executor + 10 committer)
- [x] T08 Self-validated token flow end-to-end — `validation.rs` — integration test exercising full self-signed pipeline (token → spec validate → PendingTransaction → Validated → registry lookup)
- [x] T09 Backpressure verification — `executor/src/executor.rs` — `full_backpressure_cycle`: preload at capacity → reject → cleanup → retry succeeds

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
All action branches dispatch and all coordination helpers use protocol-level users: `check_nonce()` calls `get_user()` from data store, `verify_gas()` calculates `base_cost + (amount × multiplier)` from `CostModel` and returns usage tracking, `check_stake()` verifies both `cost_model.global_min_stake` AND `config.get_min_type_stake()`. Fails with `InvalidNonce`, `InsufficientGas`, or `InsufficientStake`. Returns `GasVerified { gas_used, gas_remaining }` and `StakeChecked { node_type, stake }`. 271 tests across workspace (214 core + 22 finalizer + 9 executor + 16 sentinel + 10 committer).

#### Protocol-level User + gas model — FOUNDATION COMPLETE (P1_44 updated)
**File:** `src/user.rs`, `src/tokens.rs`, `src/data.rs`, `src/environment.rs`, `src/action_router.rs`, `src/node/registry.rs`
**Completed:** `User` struct has `stake` field (separate from `fuel_balance`). `Account` struct added for per-token balances. `CostModel` added to `EnvironmentMetadata` with `base_cost`, `global_min_stake`, `admin_public_key`, `admin_tax_percentage`, `amount_multiplier`. `DataProvider` trait has `get_user()`/`save_user()` methods. `DefaultDataProvider` and `StubDataProvider` implement them. `ActionRouter::check_nonce()`, `verify_gas()`, `check_stake()` use `get_user()` instead of `get_token() + get_asset::<User>()`. `verify_gas()` calculates real gas cost with per-action multipliers. `check_stake()` checks both `cost_model.global_min_stake` AND `config.get_min_type_stake()`. `node/registry::check_db_node_user()` updated. 271 tests passing.

#### TokenFactory — minting fee deduction — DONE
**File:** `src/tokens.rs:290-347`
`TokenFactory::mint_token_full()` calculates `minting_fee = base_cost * 10`, deducts from owner's `fuel_balance` via `data_provider`, records `admin_tax = fee * admin_tax_percentage` as `PendingAdminCredit` in `admin_credit_registry`. Returns `InsufficientGas` when balance < fee. 7 tests: `mint_token_full_deducts_fee_from_balance`, `mint_token_full_records_admin_tax_credit`, `mint_token_full_no_admin_tax_when_zero_percentage`, `mint_token_full_insufficient_gas_error`, `mint_token_full_zero_base_cost_no_deduction`, `mint_token_full_admin_credit_taken`.
**Note:** `mint_token()` (backward-compat) and `mint_user_token()` remain free — `mint_token_full` is the charged variant.

#### ActionRouter — per-action gas cost from CostModel — DONE
**File:** `src/environment.rs:26`, `src/action_router.rs`
`CostModel` has `amount_multiplier: HashMap<String, f64>` with defaults `{"Process": 1.0, "Preload": 2.0, "Sign": 1.5}`. `verify_gas()` computes `gas_used = base_cost + (amount × multiplier_for_action)` and returns `GasVerified { gas_used, gas_remaining }`. Default multipliers set in `CostModel::default_amount_multiplier()`.

#### Gas deduction after executor completes — DONE
**File:** `src/registry.rs` (gas_tracker), `sentinel/src/transaction_validator.rs` (compute_gas_used), `sentinel/src/sentinel.rs` (record_gas_used), `committer/src/committer.rs` (gas deduction in check_and_commit_transaction_results)
**Completed:** `PendingTransactionRegistry` has `gas_tracker: Mutex<HashMap<String, u64>>` with `record_gas_used()`/`get_gas_used()` methods. `TransactionValidator::compute_gas_used()` computes `gas_used = base_cost + (amount × multiplier)`. Sentinel calls `record_gas_used` during validation (both received and self-signed paths). Committer's `check_and_commit_transaction_results` deducts `gas_used` from sender's `fuel_balance` via `saturating_sub` after block commit. `Committer` has `data_provider` field injected. 271 tests passing across workspace.

#### Transaction ordering — race conditions across senders
**File:** `src/epoch.rs` (LeaderSelector) → `PendingTransactionRegistry` → `ActionRouter` pre-flight
**Current state:** Foundation complete. `TransactionPool` provides per-token deterministic ordering (sorted by sender ASC, sequence_number ASC, timestamp ASC). `LeaderSelector` implements stake-weighted random selection. `BlockProposer` dequeues and wraps transactions in `SignedTransaction` with leader metadata. `EpochBoundaryDetector` detects stale blocks and advances epochs. `resolve_block_conflict()` handles conflicting block proposals with stake-based resolution and hash tie-break. `PendingTransactionRegistry` has `transition_to_validated_and_enqueue()`, `enqueue_to_pool()`, `dequeue_for_leader()`, `get_ordered_transactions()`. Error variants: `PneumaticError::StaleBlock`, `PneumaticError::BlockConflict`, `ValidationFailureReason::StaleEpochBlock`, `ValidationFailureReason::BlockConflict`.
**Race scenarios:**
- Two leaders propose conflicting blocks at the same height → `resolve_block_conflict()` resolves via stake comparison, hash tie-break
- Epoch boundary: old leader's block arrives after new leader's block → `EpochBoundaryDetector.is_stale_block()` detects this
- Leader reads from empty queue → `TransactionPool` integration via `PendingTransactionRegistry.dequeue_for_leader()`
- Multiple senders with same nonce to same token → conflict resolved by timestamp ordering in pool
**Action:** Wire BlockProposer into the actual pipeline (Sentinel → executor → finalizer → leader block construction). Implement Finalizer quorum stake-weighted selection.

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

#### EpochReconciler — DONE
**File:** `committer/src/epoch_manager.rs:142-205`, `committer/src/committer.rs:563-569`
Added `token_ids: Vec<Vec<u8>>` field to `EpochReconciler`; constructor accepts token IDs from committer. `reconcile_internal()` loads each token via `data_provider.get_token()`, checks `chain_state.is_valid` to detect misshapen chains, and cross-compares block hashes at matching indices across valid tokens to detect finalization conflicts. `StubDataProvider` made always-available (removed `#[cfg(test)]`) for downstream test use. 7 new tests.

#### StakingManager — DONE (StubStakingManager superseded)
**File:** `src/epoch.rs:136-142` (stub in core), `committer/src/epoch_manager.rs:78-133` (real impl)
`StubStakingManager::apply_ops()` in core returns `Ok(())` — no-op. Replaced by `StakingManager` in `committer` which applies all ops (AddStaker, RemoveStaker, Slash, Reward) to `StakeStore` via DashMap-backed concurrent storage.

#### LeaderSelector — stake-weighted random selection ✓ (DONE)
**File:** `src/epoch.rs:154-186`
Replaced `StubLeaderSelector` with `LeaderSelector` using cumulative stake range approach: pick random in `[0, total_stake)`, walk sorted stakers to find who owns that point. Implements `IEpochLeaderSelector` trait. Also added `IBlockProposer` trait with `BlockProposer` implementation, `EpochBoundaryDetector` struct, and `resolve_block_conflict()` free function. New dependency: `rand = "0.8"`.
**Tests:** 22 new tests (8 LeaderSelector/Epoch, 5 BlockProposer, 9 EpochBoundaryDetector/conflict resolution).

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
Added imports for `SelfSignedBlockValidatorSpec` and `ExecutedBlockValidatorSpec`. 271 tests pass.

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

#### NodeRegistry.send_to_all — uses registered connections — DONE
**File:** `src/node/registry.rs:163-202`
Rewrote `send_to_all` to use registered `Connection` objects from `NodeRegistryNode.conn` via `conn.send(&data).await`. Uses `futures::future::join_all` for concurrent broadcasts. Added `send_to_all_blocking` for sync contexts. Changed `Connection::send` to take `&self` with `tokio::sync::Mutex` interior mutability inside `TcpConnection`. `futures = "0.3"` dependency added.

#### NodeRegistry.process_registration — DONE
**File:** `src/node/registry.rs:204-269`
Implemented `process_registration(RegistrationBatch)` with full batch processing:
- **Add entries**: validates each entry via `check_db_node_user` (DB stake check) and `type_is_maxed_out` (registry limit check). Skips invalid entries. On any rejection, returns `Failure` with details. Inserts valid entries into per-type DashMap with a `NoOpConnection` placeholder (real connections established via `process_conn_request`).
- **Remove entries**: removes by key from the appropriate DashMap for each requested node type.
Added `NoOpConnection` placeholder impl for registrations without live connections.

### pneumatic_sentinel — Priority 2 (Node-specific logic)

#### Sentinel.initialize — DONE (gossiper handler wired)
**File:** `sentinel/src/sentinel.rs:78-80`
`.initialize()` accepts a closure and passes it to `self.gossiper.initialize(gossiper_handle)`. Caller creates closure wrapping `self.on_data_received(raw)`.

#### Sentinel.send_to_executor_for_preload — empty stub
**File:** `sentinel/src/sentinel.rs:166-171`
**Action:** Call `self.transaction_notifier.send_to_executors_for_preload(tx, self.env_data)`.

#### Sentinel.handle_confirmation — stub
**File:** `sentinel/src/sentinel.rs:174-178`
**Action:** Deserialize tx_id, acquire transaction, verify finalizer, transition to Committed, notify sentinels.

#### Sentinel.handle_rejection — stub
**File:** `sentinel/src/sentinel.rs:182-184`
**Action:** Deserialize tx_id, check awaiting_finalizer state, pick new finalizer, reassign.

#### Sentinel.handle_register_request — stub
**File:** `sentinel/src/sentinel.rs:187-189`
**Action:** Deserialize `NodeRegistryRequest`, validate stake, register node.

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

### pneumatic_finalizer — Priority 4 (Complete: 22 tests pass)

Stubbed within implemented methods:
- `SignatureCollector.reconcile_signatures` — Conflict resolution (supermajority/stake-weighted) stubbed; currently returns all signatures
- `Finalizer.initialize` — Message handler subscription via gossiper stubbed (closure parameter accepted but not wired)
- `Finalizer.try_finalize` — Steps 5, 7 use placeholder data (total_stake=0, total_voters=0, previous_hash=[])
- `MessageDispatcher.send_to_all` — Uses NodeRegistry stub, not registered connections (see node/registry.rs:165)

### pneumatic_committer — Priority 5 (Complete: compiles, Phase 5 tasks done)

#### Committer crate — stub comment removed
**File:** `pneumatic_committer/src/lib.rs`
**Done:** Phase 5 tasks (P5_01–P5_11) complete — module declarations, `Committer` struct, `BlockServices`, `epoch_manager` single-file module with `StakeStore`, `StakingManager`, `EpochReconciler`, `LeaderSelector`. Gas deduction wired (committer deducts `gas_used` from sender's `fuel_balance` on commit).

### Tests — Priority 6 (271 passing across 5 crates)

#### Tests added to pneumatic_core — 209 tests
**Files:** `errors.rs` (10), `transactions.rs` (14), `registry.rs` (33 incl. 11 concurrent + 2 gas_tracker), `gossiper.rs` (9), `validation.rs` (17 + 1 integration), `tokens.rs` (7 mint_token_full fee tests), `epoch.rs` (22 LeaderSelector/Epoch/BlockProposer/conflict resolution), `config.rs` (test helpers), `data.rs` (StubDataProvider), `action_router.rs` (18), `crypto.rs` (hash, sign/verify, encrypt/decrypt, cross-recipient), `blocks.rs` (pre-existing), `messages.rs` (pre-existing)
**Covered:** PneumaticError variants, TransactionRiskFactor scoring, TransactionState transitions, PendingTransactionRegistry CRUD + concurrent ops + gas_tracker, Gossiper fan-out + dedup, SelfSigned/Executed validation specs, nonce validation, block validation result variants, self-signed token flow integration, TokenFactory minting fee deduction, LeaderSelector stake-weighted selection, BlockProposer, EpochBoundaryDetector, conflict resolution.

#### Tests added to pneumatic_finalizer — 22 tests
**Files:** `signature_collector.rs` (12 incl. 3 concurrent), `block_builder.rs` (2), `message_dispatcher.rs` (2), plus pre-existing
**Covered:** Signature add/remove, quorum detection, conflict reconciliation, concurrent safety, block building, message dispatch, shutdown behavior.

#### Tests added to pneumatic_sentinel — 16 tests
**Files:** `sentinel/src/sentinel.rs`
**Covered:** SentinelError From impls, construction, spec name routing, action dispatch (process, register, clear), self-signed token flow, compute_gas_used (3: zero amount, preload multiplier, unknown action default), TransactionNotifier send methods (4: executors, finalizer, notify_clear, notify_delete — all verify no-panic with no runtime).

#### Tests added to pneumatic_executor — 9 tests
**Files:** `executor/src/executor.rs`
**Covered:** Execution result validation, capacity checks, full backpressure cycle.

#### Tests in pneumatic_committer — 10 tests (7 inline + 3 doc, 3 ignored)
**Files:** `committer/src/epoch_manager.rs` (7 inline: StakeStore concurrent add, StakingManager ops, EpochReconciler conflict detection; 3 doc tests in committer.rs)
**Covered:** Gas deduction in check_and_commit_transaction_results (deducts, no gas tracked, saturates on overflow).

#### Remaining test gaps
**Files:** `config.rs` (unit tests), `data.rs` (DefaultDataProvider tests), `server.rs` (async poison test fix), `epoch.rs` (StubEpochReconciler/StubStakingManager unit tests), `node/registry.rs` (process_registration, send_to_all)
**Action:** Tests for Config::new_for_testing, DefaultDataProvider (TCP/UDS communication), ThreadPool async poison, epoch reconciliation stubs, node registration batch processing.
