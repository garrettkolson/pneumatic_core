use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use pneumatic_core::contracts::{
    deploy_contract, deploy_gas, select_engine, ContractEngineRegistry, ContractError,
    CreateTokenDelta, DeployParams, ExecutionInput, ExecutionOutput, TransferDelta,
};
use pneumatic_core::crypto::{AsymCryptoProvider, HashProvider};
use pneumatic_core::data::{DataError, DataProvider};
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::errors::{PneumaticError, ValidationFailureReason};
use pneumatic_core::messages::Message;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::node::NodeRegistryType;
use pneumatic_core::registry::PendingTransactionRegistry;
use pneumatic_core::rns::identity::NodeIdentity;
use pneumatic_core::tokens::{SmartContract, Token};
use pneumatic_core::transactions::{Transaction, TransactionState};
use pneumatic_core::user::User;

/// Per-execution wall-clock backstop (Q3.2). Read from
/// `PNEUMATIC_EXECUTOR_TIMEOUT_SECS` (whole seconds, default **5**). This bounds
/// a single `run_execution` so a stuck engine cannot hang a worker task or pin
/// a backpressure slot indefinitely (Phase 4 safety hardening).
fn execution_timeout_from_env() -> Duration {
    const DEFAULT_TIMEOUT_SECS: u64 = 5;
    let secs: u64 = std::env::var("PNEUMATIC_EXECUTOR_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

// ---------------------------------------------------------------------------
// Executor — transaction computation node
// ---------------------------------------------------------------------------

/// Executor receives preloaded transactions, fetches contract/user/token data,
/// executes the contract logic, validates results, and sends execution outputs
/// to the assigned Finalizer for signature collection.
///
/// Key design:
/// - **Backpressure**: configurable `max_in_flight` limits concurrent executions.
///   If the limit is reached, new transactions are rejected immediately.
/// - **Preload**: fetches all necessary data (contract, user, token, proxy auths)
///   from the DataProvider before execution.
/// - **Result**: hashes execution output and sends it to the Finalizer.
pub struct Executor {
    /// Environment ID for this executor
    env_id: String,
    /// Public key of this executor node
    public_key: Vec<u8>,
    /// Node identity — signs all outgoing execution messages
    identity: Arc<NodeIdentity>,
    /// Shared registry of connected nodes
    node_registry: Arc<NodeRegistry>,
    /// Data provider for fetching contract/user/token data
    data_provider: Arc<dyn DataProvider>,
    /// Transaction registry for state tracking
    pending_registry: Arc<PendingTransactionRegistry>,
    /// Hash provider for result hashing
    hash_provider: Arc<dyn HashProvider>,
    /// Token partition ID for data fetches (defect D3: fetch under the token
    /// partition, not the environment id).
    partition_id: String,
    /// Contract engine registry for per-token engine selection (ADR-011).
    contract_engine_registry: Arc<ContractEngineRegistry>,
    /// Per-transaction execution results (observability). Entries persist until
    /// `preload_cleanup` — independent of the backpressure slot below, so a
    /// settled task's result stays readable after its slot is freed (defect D4).
    preload_tasks: Arc<Mutex<HashMap<String, Arc<DashMap<String, Result<ExecutionResult, String>>>>>>,
    /// Backpressure slots: the transaction IDs currently executing. A slot is
    /// taken on preload and freed on settle (defect D4: the slot was previously
    /// never freed, so a leaked slot would exhaust `max_in_flight`).
    active_tasks: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Maximum number of concurrent execution tasks before backpressure kicks in
    max_in_flight: usize,
    /// Wall-clock backstop for a single execution (Q3.2, Phase 4). If the
    /// engine does not settle within this, the task is dropped and the
    /// transaction is failed with `ExecutionTimeout` rather than left in
    /// `Executing`. Env-configurable via `PNEUMATIC_EXECUTOR_TIMEOUT_SECS`
    /// (default 5 s).
    execution_timeout: Duration,
}

impl Executor {
    /// Create a new Executor with all required dependencies.
    ///
    /// `max_in_flight` controls backpressure — when this many tasks are running,
    /// new transactions will be rejected until a slot opens up.
    pub fn new(
        env_id: String,
        public_key: Vec<u8>,
        identity: Arc<NodeIdentity>,
        node_registry: Arc<NodeRegistry>,
        data_provider: Arc<dyn DataProvider>,
        pending_registry: Arc<PendingTransactionRegistry>,
        hash_provider: Arc<dyn HashProvider>,
        max_in_flight: usize,
        partition_id: String,
        contract_engine_registry: Arc<ContractEngineRegistry>,
    ) -> Self {
        Executor {
            env_id,
            public_key,
            identity,
            node_registry,
            data_provider,
            pending_registry,
            hash_provider,
            partition_id,
            contract_engine_registry,
            preload_tasks: Arc::new(Mutex::new(HashMap::new())),
            active_tasks: Arc::new(Mutex::new(std::collections::HashSet::new())),
            max_in_flight,
            execution_timeout: execution_timeout_from_env(),
        }
    }

    /// Override the per-execution wall-clock backstop (ops/test knob).
    /// `new` reads it from the environment by default.
    pub fn with_execution_timeout(mut self, timeout: Duration) -> Self {
        self.execution_timeout = timeout;
        self
    }

    /// Check if the executor is at capacity.
    /// Returns `true` if rejecting new transactions (backpressure).
    pub async fn is_at_capacity(&self) -> bool {
        let active = self.active_tasks.lock().await;
        active.len() >= self.max_in_flight
    }

    /// Get the number of currently in-flight execution tasks.
    pub async fn in_flight_count(&self) -> usize {
        let active = self.active_tasks.lock().await;
        active.len()
    }

    /// Preload data for a transaction and begin execution.
    ///
    /// Returns `Err(ExecutorError::AtCapacity)` if backpressure has kicked in.
    /// Returns `Err(ExecutorError::Registry)` if the transaction isn't found
    /// or is in a terminal state.
    ///
    /// Flow:
    /// 1. Acquire lock on the transaction in the pending registry
    /// 2. Check backpressure — reject if at capacity
    /// 3. Spawn an async task that executes the transaction
    /// 4. Track the task in `preload_tasks` for backpressure management
    pub async fn preload_for_transaction(&self, tx_id: &str) -> Result<(), ExecutorError> {
        // Step 1: Acquire lock on the transaction
        if self.pending_registry.acquire_transaction(tx_id).is_err() {
            return Err(ExecutorError::Registry(format!(
                "Transaction {} not found or in terminal state", tx_id
            )));
        }

        // Step 2: Check backpressure
        let at_capacity = self.is_at_capacity().await;
        if at_capacity {
            return Err(ExecutorError::AtCapacity {
                max_in_flight: self.max_in_flight,
                current: self.active_tasks.lock().await.len(),
            });
        }

        // Step 3: Spawn the execution task
        let task_results = Arc::new(DashMap::new());
        let results_handle = task_results.clone();

        let handle = self.clone_handle();
        handle.execute_task(tx_id.to_string(), task_results).await;

        // Step 4: Track the task — the results store (observability, persists
        // until cleanup) and the backpressure slot (freed on settle, defect
        // D4) are separate, so a settled task's result stays readable.
        self.preload_tasks.lock().await.insert(tx_id.to_string(), results_handle);
        self.active_tasks.lock().await.insert(tx_id.to_string());

        Ok(())
    }

    /// Check if a preload task has completed and collect its results.
    pub async fn preload_cleanup(&self, tx_id: &str) {
        // Remove both the results store and any (stale) backpressure slot.
        self.preload_tasks.lock().await.remove(tx_id);
        self.active_tasks.lock().await.remove(tx_id);
    }

    /// Ingest a Preload message body from the Sentinel and begin execution.
    ///
    /// The Preload body is the rmp-serialized `Transaction` (the sentinel's
    /// `send_to_executors_for_preload`), not raw tx_id bytes. The executor
    /// reads the transaction from **its own** pending registry during
    /// execution, so the ingested tx is registered here (Preloaded state)
    /// before `preload_for_transaction` is called.
    pub async fn ingest_preload(&self, body: &Vec<u8>) -> Result<(), ExecutorError> {
        let tx: Transaction = deserialize_rmp_to(body).map_err(|e| ExecutorError::Encoding(e))?;

        // Register in this executor's own registry (execution reads from here).
        if self.pending_registry.contains(&tx.id) {
            if let Ok(mut entry) = self.pending_registry.get_transaction_mut(&tx.id) {
                if matches!(entry.state, TransactionState::Pending) {
                    entry.transition_to_preloaded(tx.clone());
                }
            }
        } else {
            self.pending_registry
                .register_pending(tx.id.clone())
                .map_err(|e| ExecutorError::Registry(e.to_string()))?;
            if let Ok(mut entry) = self.pending_registry.get_transaction_mut(&tx.id) {
                entry.transition_to_preloaded(tx.clone());
            }
        }

        self.preload_for_transaction(&tx.id).await
    }

    fn clone_handle(&self) -> ExecutorHandle {
        ExecutorHandle {
            env_id: self.env_id.clone(),
            public_key: self.public_key.clone(),
            identity: self.identity.clone(),
            node_registry: self.node_registry.clone(),
            data_provider: self.data_provider.clone(),
            pending_registry: self.pending_registry.clone(),
            hash_provider: self.hash_provider.clone(),
            partition_id: self.partition_id.clone(),
            contract_engine_registry: self.contract_engine_registry.clone(),
            preload_tasks: self.preload_tasks.clone(),
            active_tasks: self.active_tasks.clone(),
            execution_timeout: self.execution_timeout,
        }
    }
}

// ---------------------------------------------------------------------------
// ExecutorHandle — lightweight clone for use in spawned tasks
// ---------------------------------------------------------------------------

/// A lightweight handle to the Executor, designed for use in async tasks.
/// Stores individual config values instead of the full Config struct to avoid
/// the need for Clone on Config.
#[derive(Clone)]
struct ExecutorHandle {
    env_id: String,
    public_key: Vec<u8>,
    identity: Arc<NodeIdentity>,
    node_registry: Arc<NodeRegistry>,
    data_provider: Arc<dyn DataProvider>,
    pending_registry: Arc<PendingTransactionRegistry>,
    hash_provider: Arc<dyn HashProvider>,
    partition_id: String,
    contract_engine_registry: Arc<ContractEngineRegistry>,
    /// Shared results store (observability) — persists until `preload_cleanup`.
    preload_tasks: Arc<Mutex<HashMap<String, Arc<DashMap<String, Result<ExecutionResult, String>>>>>>,
    /// Shared backpressure slots — the spawned task frees its slot on settle
    /// (defect D4) by removing its id here.
    active_tasks: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Per-execution wall-clock backstop (Q3.2), carried from the `Executor`.
    execution_timeout: Duration,
}

impl ExecutorHandle {
    /// Spawn an async execution task for a transaction.
    ///
    /// The task records its outcome (success **or** failure) in the per-tx
    /// results map and, on settle, frees its backpressure slot (defect D4:
    /// the slot was previously only freed by an explicit `preload_cleanup`, so
    /// a leaked slot would eventually exhaust `max_in_flight`).
    async fn execute_task(
        self,
        tx_id: String,
        results: Arc<DashMap<String, Result<ExecutionResult, String>>>,
    ) {
        tokio::spawn(async move {
            // Wall-clock backstop (Q3.2, Phase 4): a single execution must settle
            // within `execution_timeout` or it is dropped and the transaction is
            // failed — a stuck engine must not hang the task or pin its slot.
            let result =
                match tokio::time::timeout(self.execution_timeout, self.run_execution(&tx_id)).await {
                    Ok(r) => r,
                    Err(_) => {
                        // Timeout: fail the transaction so it is not left in
                        // `Executing`, and record the wall-clock backstop firing.
                        // `get_transaction` only exposes Validated-state txs, so
                        // pull the tx out of its current (non-terminal) state.
                        let reasons = vec![ValidationFailureReason::ExecutionTimeout];
                        let tx_to_fail = self
                            .pending_registry
                            .get_transaction_mut(&tx_id)
                            .ok()
                            .and_then(|entry| match &entry.state {
                                TransactionState::Preloaded { transaction } => {
                                    Some(transaction.clone())
                                }
                                TransactionState::Validated { transaction, .. } => {
                                    Some(transaction.clone())
                                }
                                TransactionState::Executing { transaction } => {
                                    Some(transaction.clone())
                                }
                                _ => None,
                            });
                        if let Some(tx) = tx_to_fail {
                            if let Ok(mut entry) = self.pending_registry.get_transaction_mut(&tx_id) {
                                entry.transition_to_failed(tx, reasons.clone());
                            }
                        }
                        log::error!(
                            "execution of {} timed out after {:?}; failed with {:?}",
                            tx_id,
                            self.execution_timeout,
                            reasons
                        );
                        Err(ExecutorError::ExecutionTimeout)
                    }
                };
            match &result {
                Ok(exec_result) => {
                    results.insert(tx_id.clone(), Ok(exec_result.clone()));
                }
                Err(e) => {
                    // Make execution failures observable (previously dropped).
                    log::warn!("execution of {} failed: {:?}", tx_id, e);
                    results.insert(tx_id.clone(), Err(format!("{:?}", e)));
                }
            }
            // D4: free the backpressure slot now that the task has settled. The
            // results entry in `preload_tasks` is left in place (observability)
            // and is removed only by an explicit `preload_cleanup`.
            self.active_tasks.lock().await.remove(&tx_id);
        });
    }

    /// Run the full execution pipeline for a single transaction.
    async fn run_execution(&self, tx_id: &str) -> Result<ExecutionResult, ExecutorError> {
        // Step 1: Load the pending transaction from the registry
        let entry = self.pending_registry.get_transaction_mut(tx_id)
            .map_err(|e| match e {
                pneumatic_core::errors::PneumaticError::Registry(msg) => {
                    ExecutorError::Registry(msg)
                }
                other => {
                    ExecutorError::Registry(format!("{:?}", other))
                }
            })?;
        let transaction = match &entry.state {
            pneumatic_core::transactions::TransactionState::Preloaded { transaction } => {
                transaction.clone()
            }
            pneumatic_core::transactions::TransactionState::Validated { transaction, .. } => {
                transaction.clone()
            }
            pneumatic_core::transactions::TransactionState::Executing { transaction } => {
                transaction.clone()
            }
            _ => {
                return Err(ExecutorError::InvalidState(format!(
                    "Transaction {} in terminal state", tx_id
                )))
            }
        };
        drop(entry);

        // Step 2: Transition to Executing state
        {
            if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
                entry.transition_to_executing(transaction.clone());
            }
        }

        // Steps 3-5: fetch the target token + sender, then execute. A
        // `DeployContract` tx is a protocol op (ADR-015, Phase 6) that creates
        // a token, so it has no target token to fetch (step 3) and no engine
        // to select (step 5); it is handled by `execute_deploy` before the
        // normal contract path.
        let execution_output = if transaction.action == "DeployContract" {
            self.execute_deploy(&transaction)
        } else {
            // Step 3: Fetch the token asset under the token partition (defect
            // D3: the data store is addressed by the partition key, not the
            // env id).
            let token = self
                .data_provider
                .get_token(&transaction.token_id, &self.partition_id)
                .map_err(ExecutorError::Data)?;

            // Step 4: Fetch the sender's protocol state (defect D6: the
            // fetched user data is the execution input, no longer discarded).
            let user = self
                .data_provider
                .get_user(&transaction.sender, &self.partition_id)
                .map_err(ExecutorError::Data)?;

            // Step 5: Execute the contract via its selected engine (defect
            // D1: real dispatch replaces the identity stub).
            self.execute_contract(&transaction, &token, &user).await
        };

        // A contract-level failure (unknown engine, missing contract, gas
        // exhaustion, revert, or a deploy failure) fails the transaction with
        // the engine's reason — the tx transitions to `Failed`, it is not
        // just an early return.
        let execution_output = match execution_output {
            Ok(output) => output,
            Err(ExecutorError::Validation(reasons)) => {
                if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
                    entry.transition_to_failed(transaction.clone(), reasons.clone());
                }
                return Err(ExecutorError::Validation(reasons));
            }
            Err(e) => return Err(e),
        };

        // Step 6: Create intermediate result. The output hash is computed
        // BEFORE validation (step 7): `validate_execution_result` rejects an
        // empty `result_hash`, so hashing must precede it — the old ordering
        // validated an always-empty hash and failed every execution.
        let final_result = ExecutionResult {
            transaction_id: tx_id.to_string(),
            result_data: execution_output.result_data.clone(),
            result_hash: self.hash_provider.hash(&execution_output.result_data),
            gas_used: execution_output.gas_used,
        };

        // Step 7: Validate execution results
        let validation_result = validate_execution_result(&transaction, &final_result);
        if let Err(reasons) = validation_result {
            // Transition to Failed state
            if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
                entry.transition_to_failed(transaction, reasons.clone());
            }
            return Err(ExecutorError::Validation(reasons));
        }

        // Step 9: Get finalizer key from validation result
        let finalizer_key = self.get_finalizer_key(tx_id);

        // Step 10: Transition to Finalizing state
        {
            if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
                entry.transition_to_finalizing(transaction.clone(), finalizer_key.clone());
            }
        }

        // Step 11: Send execution result to the Finalizer. The finalizer
        // needs the transaction registered in its own pending registry before
        // it can process the "Sign" vote (its optimistic-finality path loads
        // the tx from that registry), so the executor also emits a "Preload"
        // carrying the serialized transaction. Both ride the same RNS link to
        // each finalizer, so the "Preload" is ordered before the "Sign".
        let tx_bytes = serialize_to_bytes_rmp(&transaction).map_err(ExecutorError::Encoding)?;
        if let Err(e) = self
            .send_to_finalizer(
                tx_id,
                tx_bytes,
                final_result.result_data.clone(),
                final_result.result_hash.clone(),
            )
            .await
        {
            // Transition to Failed state on send failure
            if let Ok(mut entry) = self.pending_registry.get_transaction_mut(tx_id) {
                entry.transition_to_failed(
                    transaction.clone(),
                    vec![ValidationFailureReason::ContractNotFound],
                );
            }
            return Err(e);
        }

        Ok(final_result)
    }

    /// Execute the contract for `tx` on `token` via its selected engine
    /// (defect D1: real dispatch replaces the identity stub).
    ///
    /// The token's `asset_data` is the deployed [`SmartContract`] (deterministic
    /// 1:1 contract-token model, ADR-014). The engine is chosen per token by
    /// [`select_engine`] (ADR-011): the `contract_engine` metadata key names the
    /// engine, non-contract tokens default to `"Transfer"`, and contract tokens
    /// without a key fail closed.
    ///
    /// Phase 4 safety: the engine runs on a **blocking thread** (so a stuck
    /// engine cannot block the async worker), is bounded by the wall-clock
    /// backstop [`Self::execution_timeout`] (Q3.2), and its panics are caught
    /// with `catch_unwind` so a buggy engine fails the transaction deterministically
    /// instead of unwinding the task.
    async fn execute_contract(
        &self,
        tx: &Transaction,
        token: &Token,
        user: &User,
    ) -> Result<ExecutionOutput, ExecutorError> {
        // The token asset is the deployed contract (1:1 model, ADR-014).
        // Fail closed if the token carries no contract asset.
        let contract: SmartContract = token
            .get_asset::<SmartContract>()
            .ok_or_else(|| {
                ExecutorError::Validation(vec![ValidationFailureReason::ContractNotFound])
            })?;

        // Per-token engine selection (ADR-011).
        let engine = select_engine(&self.contract_engine_registry, token)?;

        // The engine is sync and may be slow/buggy, so it runs on a blocking
        // thread across an ownership boundary. Clone the inputs for that boundary.
        let tx_id = tx.id.clone();
        let tx = tx.clone();
        let contract = contract.clone();
        let user = user.clone();
        let token = token.clone();

        // `spawn_blocking` keeps the engine off the async worker (so the Q3.2
        // wall-clock backstop in `execute_task` can fire against a stuck engine),
        // and `catch_unwind` isolates panics so a buggy engine fails the
        // transaction deterministically instead of unwinding the task.
        let join = tokio::task::spawn_blocking(move || {
            let input = ExecutionInput {
                tx: &tx,
                contract: &contract,
                sender_state: &user,
                token: &token,
                gas_limit: tx.gas_limit,
            };
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.execute(&input)))
        });

        match join.await {
            // Success: the engine produced an output.
            Ok(Ok(Ok(output))) => Ok(output),
            // The engine returned a `ContractError`.
            Ok(Ok(Err(e))) => Err(ExecutorError::from(e)),
            // `catch_unwind` caught a panic (buggy engine).
            Ok(Err(_panic)) => {
                log::error!(
                    "contract engine panicked while executing tx {:?}; failing the transaction",
                    tx_id
                );
                Err(ExecutorError::Validation(vec![
                    ValidationFailureReason::ContractExecutionFailed,
                ]))
            }
            // The blocking task failed to join (should be rare now that panics
            // are caught inside the thread).
            Err(_join_err) => {
                log::error!(
                    "contract engine task for tx {:?} failed to join; failing the transaction",
                    tx_id
                );
                Err(ExecutorError::Validation(vec![
                    ValidationFailureReason::ContractExecutionFailed,
                ]))
            }
        }
    }

    /// Execute a `DeployContract` protocol op (ADR-015, Phase 6). There is no
    /// target token (it is being created) and no engine to select. The op:
    /// 1. parses the `DeployParams` from the tx payload;
    /// 2. checks the deployment gas fits the tx's `gas_limit` (if set);
    /// 3. derives the deterministic token id and builds a canonical
    ///    `CreateTokenDelta`;
    /// 4. emits the delta in `result_data` (the executor stays pure, ADR-013 —
    ///    the data service applies the delta at commit, idempotently).
    fn execute_deploy(&self, tx: &Transaction) -> Result<ExecutionOutput, ExecutorError> {
        // 1. Parse the deployment parameters from the payload.
        let params: DeployParams = deserialize_rmp_to(&tx.payload).map_err(|_| {
            ExecutorError::Validation(vec![ValidationFailureReason::ContractDeployFailed])
        })?;

        // 2. The deployment gas must fit the tx's gas limit (if a limit was
        // set). `gas_limit == 0` means "no limit" (mirrors the engine path).
        let gas = deploy_gas(params.bytecode.len());
        if tx.gas_limit > 0 && gas > tx.gas_limit {
            return Err(ExecutorError::Validation(vec![
                ValidationFailureReason::GasLimitExceeded,
            ]));
        }

        // 3. The op: derive the deterministic token id and build the delta.
        // The nonce is the sender's tx sequence number (`User.nonce`, per
        // sender). A reused nonce yields the same id (idempotent apply) and is
        // rejected upstream by the `DeployValidationSpec` nonce check.
        let delta: CreateTokenDelta = deploy_contract(
            &tx.sender,
            tx.sequence_number as u64,
            &params,
            &*self.hash_provider,
        )
        .map_err(ExecutorError::from)?;

        // 4. Emit the delta in result_data (rmp-canonical). The data service
        // applies it at commit.
        let result_data = serialize_to_bytes_rmp(&delta).map_err(|_| {
            ExecutorError::Validation(vec![ValidationFailureReason::ContractDeployFailed])
        })?;

        Ok(ExecutionOutput {
            result_data,
            gas_used: gas,
        })
    }

    /// Get the finalizer public key from the transaction's validation result.
    fn get_finalizer_key(&self, tx_id: &str) -> Vec<u8> {
        match self.pending_registry.get_transaction_mut(tx_id) {
            Ok(entry) => {
                if let pneumatic_core::transactions::TransactionState::Validated { validation, .. } =
                    &entry.state
                {
                    return validation.finalizer_public_key.clone();
                }
            }
            Err(_) => {}
        }
        vec![]
    }

    /// Send the execution result to the Finalizers.
    ///
    /// Two messages ride to each finalizer, in order (same RNS link ⇒ FIFO):
    ///
    /// 1. `"Preload"` carrying the serialized transaction — registers the tx
    ///    in the finalizer's own pending registry, which its
    ///    optimistic-finality path loads from (without it, the "Sign" vote
    ///    below is rejected with "not found in registry").
    /// 2. `"Sign"` — the executor's signed vote. The executor is a *voter*
    ///    in the finalizer's signature collection (audit C1): it signs the
    ///    execution-output hash with its own identity key and emits a
    ///    `TransactionSignature` — the contract `Finalizer::handle_signature`
    ///    implements (envelope auth + registered-Executor gate, then the
    ///    inner signature check over `transaction_hash`).
    ///
    /// `current_stake` is sent as 0: the finalizer stamps the real stake from
    /// the epoch snapshot (C1 — never trust a self-reported stake).
    async fn send_to_finalizer(
        &self,
        tx_id: &str,
        tx_bytes: Vec<u8>,
        execution_result: Vec<u8>,
        result_hash: Vec<u8>,
    ) -> Result<(), ExecutorError> {
        // Look up the finalizer nodes in the registry
        let finalizer_nodes = self
            .node_registry
            .get_nodes(&NodeRegistryType::Finalizer)
            .ok_or_else(|| ExecutorError::NoFinalizers("No finalizers registered".to_string()))?;

        if finalizer_nodes.is_empty() {
            return Err(ExecutorError::NoFinalizers(
                "No finalizers registered".to_string(),
            ));
        }

        // (1) Preload: register the transaction in the finalizer's registry.
        // The finalizer's `handle_preload` deserializes the body as a
        // `Transaction` and stores it; the sender is this executor.
        let preload_message = Message::signed(
            self.env_id.clone(),
            "Preload",
            tx_bytes,
            None,
            &self.identity,
        )?;
        let preload_payload =
            serialize_to_bytes_rmp(&preload_message).map_err(|e| ExecutorError::Encoding(e))?;
        self.node_registry
            .send_to_all(preload_payload, &NodeRegistryType::Finalizer)
            .await;

        // (2) Sign: the executor's signed vote. Sign the execution-output
        // hash with this executor's identity key — the finalizer's inner
        // check verifies it over `transaction_hash`.
        let signature = self
            .identity
            .ed25519
            .sign_data(&result_hash)
            .map_err(|e| ExecutorError::Crypto(e))?;

        let vote = pneumatic_core::transactions::TransactionSignature {
            transaction_id: tx_id.as_bytes().to_vec(),
            env_id: self.env_id.as_bytes().to_vec(),
            transaction_hash: result_hash,
            signature,
            current_stake: 0, // stamped by the finalizer from the epoch snapshot (C1)
        };

        let msg_body =
            serialize_to_bytes_rmp(&vote).map_err(|e| ExecutorError::Encoding(e))?;
        let _ = execution_result; // the result bytes are covered by their hash

        // Sign with our own identity — the receiver verifies against the
        // sender's registered key, never the destination's.
        let message = Message::signed(
            self.env_id.clone(),
            "Sign",
            msg_body,
            None,
            &self.identity,
        )?;

        let payload = serialize_to_bytes_rmp(&message).map_err(|e| ExecutorError::Encoding(e))?;

        // Broadcast to all finalizers via registered connections
        self.node_registry.send_to_all(payload, &NodeRegistryType::Finalizer).await;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Execution-result validation
// ---------------------------------------------------------------------------

/// Validate an execution result against the transaction and the engine's
/// post-conditions. Pure function of the tx + result (no executor state), so
/// both the runtime path and the tests share one definition.
///
/// Checks:
/// 1. `result_data` is non-empty (the engine produced an output).
/// 2. `result_hash` is non-empty (the vote is taken over the hash).
/// 3. `gas_used` respects the sender-declared `gas_limit` (0 = no cap).
/// 4. Engine-specific: if `result_data` decodes as a canonical
///    [`TransferDelta`], its fields must agree with the transaction.
fn validate_execution_result(
    tx: &Transaction,
    result: &ExecutionResult,
) -> Result<(), Vec<ValidationFailureReason>> {
    let mut reasons: Vec<ValidationFailureReason> = Vec::new();

    if result.result_data.is_empty() {
        reasons.push(ValidationFailureReason::ContractExecutionFailed);
    }
    if result.result_hash.is_empty() {
        reasons.push(ValidationFailureReason::MissingResultHash);
    }
    if tx.gas_limit > 0 && result.gas_used > tx.gas_limit {
        reasons.push(ValidationFailureReason::GasLimitExceeded);
    }

    // Transfer post-condition: decode the canonical delta (if it is one) and
    // check the fields agree with the transaction. Non-transfer outputs (e.g.
    // the Spec engine) simply do not decode as a `TransferDelta` and skip this
    // check.
    if let Ok(delta) = deserialize_rmp_to::<TransferDelta>(&result.result_data) {
        if delta.amount != tx.amount.unwrap_or(0)
            || delta.sender != tx.sender
            || delta.receiver != tx.receiver
            || delta.token_id != tx.token_id
        {
            reasons.push(ValidationFailureReason::InvalidAmount);
        }
    }

    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

// ---------------------------------------------------------------------------
// ExecutionResult — output from contract execution
// ---------------------------------------------------------------------------

/// Result of executing a transaction's contract logic.
/// Sent to the Finalizer for signature collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Transaction that was executed
    pub transaction_id: String,
    /// Raw execution output bytes
    pub result_data: Vec<u8>,
    /// SHA-256 hash of the result data
    pub result_hash: Vec<u8>,
    /// Metered gas cost from the engine (0 = not metered / legacy).
    #[serde(default)]
    pub gas_used: u64,
}

// ---------------------------------------------------------------------------
// ExecutorError — errors specific to executor operations
// ---------------------------------------------------------------------------

/// Errors that can occur during transaction execution.
#[derive(Debug)]
pub enum ExecutorError {
    /// Data provider failed to fetch contract/user/token data
    Data(DataError),
    /// Serialization/deserialization failure
    Encoding(std::io::Error),
    /// Registry operation failed (transaction not found, wrong state)
    Registry(String),
    /// Transaction was in an invalid state for execution
    InvalidState(String),
    /// Validation failed after execution
    Validation(Vec<ValidationFailureReason>),
    /// No finalizer nodes registered
    NoFinalizers(String),
    /// Signing the execution-result hash failed (vote path, audit C1)
    Crypto(PneumaticError),
    /// Backpressure: executor is at max capacity
    AtCapacity {
        max_in_flight: usize,
        current: usize,
    },
    /// The wall-clock backstop (Q3.2) fired: the engine did not settle within
    /// the per-execution timeout. The transaction is failed with
    /// [`ValidationFailureReason::ExecutionTimeout`].
    ExecutionTimeout,
}

impl From<DataError> for ExecutorError {
    fn from(e: DataError) -> Self {
        ExecutorError::Data(e)
    }
}

impl From<std::io::Error> for ExecutorError {
    fn from(e: std::io::Error) -> Self {
        ExecutorError::Encoding(e)
    }
}

impl From<PneumaticError> for ExecutorError {
    fn from(e: PneumaticError) -> Self {
        ExecutorError::Data(DataError::CryptoError(e.to_string()))
    }
}

impl From<ContractError> for ExecutorError {
    fn from(e: ContractError) -> Self {
        ExecutorError::Validation(e.to_failure_reasons())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    pub mod helpers;
    mod dispatch;
    mod lifecycle;
    mod safety;
    mod signing;
    mod validation;
    mod deploy;
}
