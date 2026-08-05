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
- [x] P1_44 Implement `ActionRouter` with utility token coordination — `action_router.rs` — implements IActionRouter trait; `handle()` delegates to `route()`; action branches: Process→nonce+gas, Preload→gas+stake(Executor), Sign→stake(Finalizer), Confirm→GasVerified, Reject→NonceUpdated(0), Register→stake(Sentinel), Clear→NonceUpdated(0), DistributeToken→TokenDispatched; utility coordination: `check_nonce()` registers sender in `PendingTransactionRegistry`, `verify_gas()` always passes (stub), `check_stake()` returns zero stake (stub); 2 builders (`new()`, `new_with_registry()`); 13 tests
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

## Phase 7: Tests (~166 passing — ~39 → ~166)

All tests use inline `#[cfg(test)] mod tests` blocks (no external `tests/` directory).
Factory helpers follow `make_*` pattern. Concurrent tests use `std::thread::spawn` with `Arc`-shared DashMaps.

- [x] T01 Add tests for TransactionState transitions — `transactions.rs` — 14 tests: lifecycle, acquire/release, state predicates
- [x] P1_Add tests for PneumaticError variants — `errors.rs` — 10 tests: From impls, risk scoring, validation error matching
- [x] P1_Add tests for PendingTransactionRegistry — `registry.rs` — 22 unit tests (CRUD, state transitions, validation result lookup) + 11 concurrent tests (atomic ops, race safety, stress)
- [x] P1_Add tests for PendingTransaction acquire/release — `registry.rs` — included in registry tests above
- [x] P1_Add tests for Gossiper — `gossiper.rs` — 9 tests: accept first, ignore duplicate, accept different, capacity, fan-out invokes-all, fan-out receives-copy, fan-out dedup-skips-all, fan-out three-handlers, fan-out concurrent-invocation
- [x] P1_Add tests for ValidationSpec — `validation.rs` — 17 tests: SelfSignedBlockValidatorSpec, ExecutedBlockValidatorSpec, ValidationSpecRegistry, nonce validation
- [x] P2_Add tests for Sentinel message routing — `sentinel/src/sentinel.rs` — 9 tests: From impls, creation, spec name routing, action dispatch, self-signed flow
- [x] P4_Add tests for SignatureCollector quorum logic — `finalizer/src/signature_collector.rs` — 3 concurrent tests: multi-thread add, duplicate rejection, quorum during concurrent adds
- [x] P4_Add tests for BlockBuilder — `finalizer/src/block_builder.rs` — 2 tests: build_signed_transaction, create_block
- [x] P4_Add tests for MessageDispatcher — `finalizer/src/message_dispatcher.rs` — 2 tests: send_to_committers, send_clear_to_sentinels
- [x] P3_Add tests for Executor — `executor/src/executor.rs` — 5 tests: validation result, backpressure cycle
- [x] T07 Migrate existing tests — all test-bearing files — total 193 tests across 4 crate targets (153 core + 9 sentinel + 22 finalizer + 9 executor)
- [x] T08 Self-validated token flow end-to-end — `validation.rs` — integration test exercising full self-signed pipeline (token → spec validate → PendingTransaction → Validated → registry lookup)
- [x] T09 Backpressure verification — `executor/src/executor.rs` — `full_backpressure_cycle`: preload at capacity → reject → cleanup → retry succeeds

---

## Stubbed Functionality Inventory

This section lists all code that is currently a **stub, placeholder, or `todo!()`** — structural scaffolding that is built but has no real implementation. Each item includes the file, line numbers, and what needs to be filled in.

### pneumatic_core — Priority 1 (Core runtime functions)

#### Crypto provider — `encrypt`/`decrypt` now implemented (P6_04) (sign/verify via ed25519-dalek)
**File:** `src/crypto.rs:74-123`
Hybrid AES-256-GCM + X25519 key exchange. Each `encrypt()` generates a fresh ephemeral keypair, derives shared secret via DH, encrypts payload. Wire format: `[32-byte ephemeral PK][ciphertext + 16-byte GCM tag]`.
**Dependencies:** `aes-gcm` 0.11.0, `x25519-dalek` 3.0.0 (with `static_secrets` + `getrandom` features).
**Cross-recipient:** `encrypt_to`/`decrypt_from` (P6_05) extends encryption to arbitrary recipients via their X25519 public key.

#### ActionRouter — routing branches now wired (P1_44), coordination stubbed
**File:** `src/action_router.rs`
All action branches now dispatch: Process→nonce+gas, Preload→gas+stake(Executor), Sign→stake(Finalizer), Confirm→GasVerified, Reject→NonceUpdated(0), Register→stake(Sentinel), Clear→NonceUpdated(0), DistributeToken→TokenDispatched. Implements `IActionRouter` trait; `handle()` delegates to `route()`.
**Stubbed:** `verify_gas()` always passes, `check_stake()` returns zero, nonce check registers sender in `PendingTransactionRegistry` but doesn't validate against real token state.
**Action:** Wire through `NodeRegistry.send_to_all()` for forwarding, implement real stake checking via staking manager, nonce validation against token sequence numbers.

#### Gossiper — handler stored and wired ✓ (DONE)
**File:** `src/gossiper.rs:23`
Handler stored as `Mutex<Option<Box<dyn Fn(Vec<u8>) + Send + Sync>>>`. The `initialize()` method stores the closure and `handle_message()` invokes it after dedup check.
**Wiring:** Sentinel: `sentinel.initialize(move |raw| { if let Err(e) = arc.on_data_received(raw) { ... } })`. Committer: wraps caller's `Fn(Message)` with deserialization. Finalizer: stub (no Gossiper field yet).

#### Gossiper — no crypto validation
**File:** `src/gossiper.rs:69`
Comment: `// TODO: validate crypto signature via AsymCryptoProvider.check_signature()`
**Note:** `encrypt`/`decrypt` are now implemented (P6_04). The remaining gap is signature validation in the deserialization path.
**Action:** After deserialization, verify `AsymCryptoProvider.check_signature(message.signature, message.body)`.

#### Gossiper — fan-out to handler delegates done (P1_42)
**File:** `src/gossiper.rs:87-92`
Handlers stored as `Vec` behind `Mutex`; `initialize()` registers first, `add_handler()` registers extras; `handle_message()` clones raw_data and invokes each sequentially.

#### EpochReconciler — always returns empty
**File:** `src/epoch.rs:125-133`
`StubEpochReconciler::reconcile()` returns `EpochReconciliation::default()` (all empty collections).
**Action:** Implement chain analysis at epoch boundaries — detect misshapen tokens, finalization conflicts.

#### StakingManager — no-op
**File:** `src/epoch.rs:136-142`
`StubStakingManager::apply_ops()` returns `Ok(())`.
**Action:** Persist AddStaker/RemoveStaker/Slash/Reward ops to data store.

#### LeaderSelector — returns empty
**File:** `src/epoch.rs:145-153`
`StubLeaderSelector::select()` returns `vec![]`.
**Action:** Stake-weighted random selection from `StakeSet`.

#### Registry — finalizer_public_key stored separately, not in validation result
**File:** `src/registry.rs:99-100`
Comment: `// validation.finalizer_public_key = finalizer_key;`
**Action:** Update `TransactionValidationResult` in `Validated` state with the finalizer key.

#### Token.get_asset_mut — returns immutable copy
**File:** `src/tokens.rs:111-122`
Returns `Option<T>` (same as `get_asset`) instead of a mutable reference.
**Action:** Return `&mut Option<Vec<u8>>` or add a `set_asset` method.

#### EnvironmentMetadataSpec — unused fields
**File:** `src/environment.rs:84-99`
`allowed_token_types`, `trans_validation_specs`, `block_validation_specs`, `sym_crypto_provider`, `serialization_provider` — none are processed in `load_from_spec`.
**Action:** Wire each field to appropriate initialization logic.

#### Config — node registry type selection
**File:** `src/config.rs:37-46`
Three `// todo` comments for determining node types, connection counts, and minimum stake.
**Action:** Parse config spec for node type selection and stake requirements.

#### Server worker — exits after one job
**File:** `src/server.rs:116-129`
The `return` inside `match mutex.recv()` exits the entire thread after processing one job instead of continuing the loop.
**Action:** Remove `return` so the loop continues processing subsequent jobs.

#### Server async poison test — hangs
**File:** `src/server.rs:252-275`
Comment: `// TODO: this test causes the test runner to hang as-is - have to fix`
**Action:** Uncomment and fix — likely needs `catch_unwind` or separate tokio runtime.

#### TcpConnection — cleanup on drop
**File:** `src/conns.rs:103`
Comment: `// TODO: initiate drop`
**Action:** Add `Drop` impl to cancel `listening_thread` and join with timeout.

#### NodeRegistry.send_to_all — creates senders on the fly
**File:** `src/node/registry.rs:164-187`
Comment: `// TODO: have to redo this to use registered conns instead of creating senders on the fly`
**Action:** Use `NodeRegistryNode.conn.send()` instead of `ConnFactory.get_sender()`.

#### NodeRegistry.process_registration — always succeeds
**File:** `src/node/registry.rs:189`
Returns `RegistrationBatchResult::Success` without processing the batch or validating entries.
**Action:** Iterate Add/Remove, insert/remove from DashMap, validate entries.

### pneumatic_sentinel — Priority 2 (Node-specific logic)

#### Sentinel.initialize — gossiper handler wiring stubbed
**File:** `sentinel/src/sentinel.rs:66-74`
**Action:** Create closure calling `self.on_data_received(raw)` and pass to `self.gossiper.initialize(closure)`.

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

#### TransactionValidator.validate_transaction — spec not actually invoked
**File:** `sentinel/src/transaction_validator.rs:33-61`
Looks up spec by action name but does NOT call `spec.validate()`. Only checks that a spec exists.
**Action:** Inject `DataProvider` to load token, call `spec.validate(tx, token, &self.env_data)`.

#### TransactionNotifier.send_to_nodes — full stub
**File:** `sentinel/src/transaction_notifier.rs:107-112`
**Action:** Inject `NodeRegistry`, look up nodes of target type, use their `Connection` to send payload.

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

### pneumatic_finalizer — Priority 4 (Complete: 19 tests pass)

Stubbed within implemented methods:
- `SignatureCollector.reconcile_signatures` — Conflict resolution (supermajority/stake-weighted) stubbed; currently returns all signatures
- `Finalizer.initialize` — Message handler subscription via gossiper stubbed (closure parameter accepted but not wired)
- `Finalizer.try_finalize` — Steps 5, 7 use placeholder data (total_stake=0, total_voters=0, previous_hash=[])
- `MessageDispatcher.send_to_all` — Uses NodeRegistry stub, not registered connections (see node/registry.rs:165)

### pneumatic_committer — Priority 5 (Complete: compiles, Phase 5 tasks done)

#### Committer crate — stub comment removed
**File:** `pneumatic_committer/src/lib.rs`
**Done:** Phase 5 tasks (P5_01–P5_11) complete — module declarations, `Committer` struct, `BlockServices`, `epoch_manager` sub-directory with `StakeStore`, `StakingManager`, `EpochReconciler`, `LeaderSelector`.

### Tests — Priority 6 (~166 passing across 4 crates)

#### Tests added to pneumatic_core — 142 tests (131 + 11 action_router; pre-existing 12 remain)
**Files:** `errors.rs` (10), `transactions.rs` (14), `registry.rs` (33), `gossiper.rs` (9), `validation.rs` (17), plus all pre-existing test-bearing files
**Covered:** PneumaticError variants, TransactionRiskFactor scoring, TransactionState transitions, PendingTransaction acquire/release, PendingTransactionRegistry CRUD + concurrent ops, Gossiper fan-out + dedup, SelfSigned/Executed validation specs, nonce validation, block validation result variants, self-signed token flow integration.

#### Tests added to pneumatic_finalizer — 22 tests
**Files:** `signature_collector.rs` (12 incl. 3 concurrent), `block_builder.rs` (2), `message_dispatcher.rs` (2), plus pre-existing
**Covered:** Signature add/remove, quorum detection, conflict reconciliation, concurrent safety, block building, message dispatch, shutdown behavior.

#### Tests added to pneumatic_sentinel — 9 tests
**Files:** `sentinel/src/sentinel.rs`
**Covered:** SentinelError From impls, construction, spec name routing, action dispatch (process, register, clear), self-signed token flow.

#### Tests added to pneumatic_executor — 9 tests
**Files:** `executor/src/executor.rs`
**Covered:** Execution result validation, capacity checks, full backpressure cycle.

#### Remaining test gaps
**Files:** `crypto.rs`, `blocks.rs`, `config.rs`, `data.rs`, `tokens.rs`, `server.rs`, `epoch.rs`
**Action:** Tests for HashProvider (BasicHashProvider has partial coverage via block tests), DataProvider trait, Token creation/comparison, ThreadPool (sync worker exits early — see Server worker stub), epoch reconciliation (stubbed).
