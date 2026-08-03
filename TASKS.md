# Pneumatic Rust Implementation Checklist

Tracks all tasks for implementing the full pneumatic blockchain protocol in Rust. Maps the C# reference `pneuma/` onto the Rust workspace.

**Format:** `- [ ] [task-id] Description — [file] — refs C#:path`

---

## Phase 0: Workspace

- [ ] W01 Create workspace Cargo.toml — `Cargo.toml`

## Phase 1: pneumatic_core Core Library

### 1.0 Error Types (Foundation)

- [ ] P0_01 Add `PneumaticError` enum covering all failure paths — `errors.rs`
- [ ] P0_02 Verify `ConnError`, `DataError` integrate into PneumaticError

### 1.1 Transaction Lifecycle (Explicit State Machine)

- [ ] P1_01 Add `TransactionState` enum (Pending → Preloaded → Validated → [Executing →] Finalizing → Committed/Failed) — `transactions.rs`
- [ ] P1_02 Add `PendingTransaction` with state machine transitions — `transactions.rs`
- [ ] P1_03 Implement `PendingTransaction::acquire()` / `release()` with lock count — `transactions.rs`

### 1.2 Transaction & SignedTransaction Model

- [ ] P1_04 Add `Transaction` struct with all fields — `transactions.rs` — refs: C# Transaction.cs
- [ ] P1_05 Add `Bid` struct — `transactions.rs` — refs: C# Bid.cs
- [ ] P1_06 Add `TransactionValidationResult` — `transactions.rs` — refs: C# TransactionValidationResult.cs
- [ ] P1_07 Add `ValidationFailureReason` enum — `transactions.rs` — refs: C# TransactionValidationResult.cs
- [ ] P1_08 Add concrete `TransactionRiskFactor` with metrics (affected_parties, amount, is_contract, is_multi_party) — `transactions.rs`
- [ ] P1_09 Refactor `SignedTransaction` to C# signature model — `transactions.rs` — refs: C# SignedTransaction.cs
- [ ] P1_10 Update `TransactionCommit` with serialized block data — `transactions.rs`

### 1.3 Token & Blockchain Refactoring

- [ ] P1_11 Add fields to Token (id, sequence_number, is_self_verified, is_non_transferable, block_validation_spec_name, environment_id) — `tokens.rs` — refs: C# Token.cs
- [ ] P1_12 Add `Token::create_block(metadata, signed_tx)` — `tokens.rs` — refs: C# Token.cs:40-48
- [ ] P1_13 Convert `Token::validate_block` → spec lookup — `tokens.rs` — refs: C# Token.cs:51-59
- [ ] P1_14 Implement `Token::commit_block` — `tokens.rs` — refs: C# Token.cs:61-76
- [ ] P1_15 Add `Blockchain` metadata fields — `blocks.rs` — refs: C# Blockchain.cs
- [ ] P1_16 Refactor `Blockchain::create_hash` → HashProvider — `blocks.rs` — refs: C# Blockchain.cs:71-75

### 1.4 ValidationSpec System (CRITICAL)

- [ ] P1_17 Create `validation.rs` with `TransactionValidationSpec` trait — `validation.rs` — refs: C# TransactionValidationSpec.cs
- [ ] P1_18 Implement `SelfSignedBlockValidatorSpec` (checks owner signature, enables skip path) — `validation.rs` — refs: C# SelfSignedBlockValidatorSpec.cs
- [ ] P1_19 Implement `ExecutedBlockValidatorSpec` — `validation.rs` — refs: C# ExecutedBlockValidatorSpec.cs
- [ ] P1_20 Add `block_validator_specs` HashMap to EnvironmentMetadata — `environment.rs` — refs: C# EnvironmentMetadataSpec.cs

### 1.5 PendingTransactionRegistry

- [ ] P1_21 Create `registry.rs` with `PendingTransactionRegistry` (DashMap-backed) — `registry.rs` — refs: C# PendingTransactionRegistry.cs
- [ ] P1_22 All methods return `Result` (never `Option`) — `registry.rs`

### 1.6 TransactionSignatureRegistry

- [ ] P1_23 Implement `TransactionSignatureRegistry` — `registry.rs` — refs: C# TransactionSignatureRegistry.cs

### 1.7 EpochManager Types (Concrete Structure)

- [ ] P1_24 Add `Epoch` struct — `epoch.rs` — refs: C# Epoch.cs
- [ ] P1_25 Add `StakingOp` enum — `epoch.rs`
- [ ] P1_26 Add `EpochReconciliation` struct — `epoch.rs` — refs: C# IEpochReconciler.cs
- [ ] P1_27 Create `IEpochReconciler` trait (returns reconciliation data) — `epoch.rs`
- [ ] P1_28 Create `IStakingManager` trait (applies ops) — `epoch.rs` — refs: C# IStakingManager.cs
- [ ] P1_29 Create `IEpochLeaderSelector` trait — `epoch.rs` — refs: C# IEpochLeaderSelector.cs
- [ ] P1_30 Stub implementations that return empty structures — `epoch.rs`

### 1.8 TokenFactory + Token Types

- [ ] P1_31 Implement `TokenFactory::mint_token` — `tokens.rs` — refs: C# TokenFactory.cs
- [ ] P1_32 Add `SmartContract`, `ContractProxyAuthorization`, `User` structs — `tokens.rs`
- [ ] P1_33 Add `ContractToken: Token` — `tokens.rs` — refs: C# ContractToken.cs
- [ ] P1_34 Add `ProxyAuthToken: Token` — `tokens.rs` — refs: C# ProxyAuthToken.cs
- [ ] P1_35 Add `UserToken: Token` — `tokens.rs` — refs: C# UserToken.cs

### 1.9 Message & BlockValidatorSpec

- [ ] P1_36 Add `MessageBody<T>` — `messages.rs` — refs: C# MessageBody.cs
- [ ] P1_37 Rename `Message.env_id` → `chain_id` — `messages.rs` — refs: C# Message.cs
- [ ] P1_38 Create `BlockValidatorSpec` trait — `validation.rs` — refs: C# BlockValidatorSpec.cs
- [ ] P1_39 Add `SelfSignedBlockValidatorSpec` for blocks — `validation.rs` — refs: C# SelfSignedBlockValidatorSpec.cs
- [ ] P1_40 Add `ExecutedBlockValidatorSpec` for blocks — `validation.rs` — refs: C# ExecutedBlockValidatorSpec.cs

### 1.10 Gossiper

- [ ] P1_41 Implement `Gossiper` struct with config TTL cache — `gossiper.rs` — refs: C# Gossiper.cs
- [ ] P1_42 Copy payload to each handler delegate (C# TODO) — `gossiper.rs`

### 1.11 IActionRouter

- [ ] P1_43 Create `IActionRouter` trait — `action_router.rs` — refs: C# IActionRouter.cs
- [ ] P1_44 Implement `ActionRouter` with utility token coordination (nonce, gas, stake) — `action_router.rs`
- [ ] P1_45 Create `ActionRouterResult` type — `action_router.rs`

### 1.12 Remove Validator

- [ ] P1_46 Remove `Validator` from `NodeRegistryType` enum — `node.rs` — dead code in C#

### 1.13 HashProvider

- [ ] P1_47 Create `IHashProvider` trait — `crypto.rs` — refs: C# IHashProvider.cs

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

- [ ] P3_01 Create executor crate — `executor/Cargo.toml` — structural
- [ ] P3_02 Implement `Executor` struct with `max_in_flight` (backpressure) — `executor/src/executor.rs` — refs: C# Executor.cs
- [ ] P3_03 Implement `initialize`, `on_data_received` — `executor/src/executor.rs` — refs: C# Executor.cs:27-30, 71-97
- [ ] P3_04 Implement `preload_for_transaction` — `executor/src/executor.rs` — refs: C# Executor.cs:33-43
- [ ] P3_05 Implement `process_transaction` — `executor/src/executor.rs` — refs: C# Executor.cs:45-67
- [ ] P3_06 Implement backpressure check (reject if overloaded) — `executor/src/executor.rs`
- [ ] P3_07 Implement preload task cleanup — `executor/src/executor.rs`

## Phase 4: pneumatic_finalizer Crate (Split from C# Monolithic Design)

### 4.1 Finalizer

- [ ] P4_01 Create finalizer crate — `finalizer/Cargo.toml` — structural
- [ ] P4_02 Implement `Finalizer` struct with SignatureCollector/BlockBuilder/MessageDispatcher — `finalizer/src/finalizer.rs` — refs: C# Finalizer.cs
- [ ] P4_03 Implement `initialize`, `handle_preload`, `handle_signature`, `try_finalize` — `finalizer/src/finalizer.rs` — refs: C# Finalizer.cs
- [ ] P4_04 Implement shutdown handling — `finalizer/src/finalizer.rs` — refs: C# Finalizer.cs:47-50

### 4.2 SignatureCollector (Split from C# TransactionReconciler)

- [ ] P4_05 Create `signature_collector.rs` — `finalizer/src/signature_collector.rs`
- [ ] P4_06 Implement `add_signature` — `finalizer/src/signature_collector.rs` — refs: C# TransactionReconciler.cs:63-73
- [ ] P4_07 Implement `check_quorum` — `finalizer/src/signature_collector.rs` — refs: C# TransactionReconciler.cs:75-84
- [ ] P4_08 Implement `reconcile_signatures` (supermajority, stake-weighted) — `finalizer/src/signature_collector.rs` — refs: C# TransactionReconciler.cs:138-200
- [ ] SignatureCollector returns data, does NOT build blocks or send messages

### 4.3 BlockBuilder (Split from C# TransactionReconciler)

- [ ] P4_09 Create `block_builder.rs` — `finalizer/src/block_builder.rs`
- [ ] P4_10 Implement `build_signed_transaction` — `finalizer/src/block_builder.rs` — refs: C# TransactionReconciler.cs:176-199
- [ ] P4_11 Implement `sign_finalizer_block` — `finalizer/src/block_builder.rs` — refs: C# TransactionReconciler.cs:202-225
- [ ] P4_12 Implement `create_block` — `finalizer/src/block_builder.rs` — refs: C# TransactionReconciler.cs:96-104

### 4.4 MessageDispatcher (Split from C# TransactionReconciler)

- [ ] P4_13 Create `message_dispatcher.rs` — `finalizer/src/message_dispatcher.rs`
- [ ] P4_14 Implement `send_to_committers` — `finalizer/src/message_dispatcher.rs` — refs: C# TransactionReconciler.cs:107-111
- [ ] P4_15 Implement `send_clear_to_sentinels` — `finalizer/src/message_dispatcher.rs` — refs: C# TransactionReconciler.cs:113-122

## Phase 5: Refactor pneumatic_committer

### 5.1 Committer

- [ ] P5_01 Update `pneumatic_committer/Cargo.toml` — `pneumatic_committer/Cargo.toml` — structural
- [ ] P5_02 Refactor `Committer` struct with gossiper, block_services, token_distributor — `pneumatic_committer/src/lib.rs` — refs: C# Committer.cs:32-54
- [ ] P5_03 Implement `check_and_commit_transaction_results` — `pneumatic_committer/src/lib.rs` — refs: C# Committer.cs:66-94
- [ ] P5_04 Simplify `validate_transaction_message` — `pneumatic_committer/src/lib.rs` — refs: C# Committer.cs:97-103
- [ ] P5_05 Use Result throughout (no silent logger.log failures) — `pneumatic_committer/src/lib.rs`

### 5.2 EpochManager

- [ ] P5_06 Create `epoch_manager/` directory — `pneumatic_committer/src/epoch_manager/mod.rs` — structural
- [ ] P5_07 Implement `CommitterBlockServices` — `pneumatic_committer/src/epoch_manager/committer_block_services.rs` — refs: C# CommitterBlockServices.cs
- [ ] P5_08 Implement `StakingManager` with concrete types — `pneumatic_committer/src/epoch_manager/staking_manager.rs` — refs: C# StakingManager.cs
- [ ] P5_09 Implement `EpochReconciler` — `pneumatic_committer/src/epoch_manager/epoch_reconciler.rs` — refs: C# EpochReconciler.cs
- [ ] P5_10 Implement `LeaderSelector` (stubbed) — `pneumatic_committer/src/epoch_manager/leader_selector.rs`

### 5.3 Main

- [ ] P5_11 Update `main.rs` — `pneumatic_committer/src/main.rs` — structural

## Phase 6: Crypto Implementation

- [ ] P6_01 Implement `RsaCryptoProvider` (encrypt/decrypt/sign/check) using ring — `crypto.rs` — refs: C# IAsymmetricalEncryptionProvider.cs
- [ ] P6_02 Implement `BasicHashProvider::hash` using ring/SHA-256 — `crypto.rs` — refs: C# IHashProvider.cs
- [ ] P6_03 Switch EnvironmentMetadata crypto provider from `Mutex` → `RwLock` — `environment.rs`

## Phase 7: Tests

- [ ] T01 Add tests for TransactionState transitions — `transactions.rs` tests
- [ ] P1_Add tests for PendingTransactionRegistry — `registry.rs` tests
- [ ] P1_Add tests for PendingTransaction acquire/release — `registry.rs` tests
- [ ] P0_Add tests for PneumaticError variants
- [ ] P1_Add tests for Gossiper dedup — `gossiper.rs` tests
- [ ] P1_Add tests for ValidationSpec — `validation.rs` tests
- [ ] P2_Add tests for Sentinel message routing — `sentinel/src/sentinel.rs` tests
- [ ] P4_Add tests for SignatureCollector quorum logic — `finalizer/src/signature_collector.rs` tests
- [ ] T07 Migrate existing 28 tests from pneumatic_core — all test-bearing files
- [ ] T08 End-to-end: verify self-validated token flow — integration test
- [ ] T09 Verify backpressure: executor rejects when overloaded — integration test
