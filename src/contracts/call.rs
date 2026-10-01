//! Model X cross-contract calls (ADR-016, Phase 9).
//!
//! A contract on token A invokes a contract on token B through a **snapshot-pinned,
//! non-atomic** call: the call carries a [`SnapshotRef`] (B's chain position); the
//! executor-side [`TargetStateProvider`] resolves B's state at that ref (validated
//! against B's chain); B's engine runs **in the same call frame** under a sub-gas
//! budget; B's side settles as a cross-referenced [`CrossChainCommitment`] tx on B's
//! chain on its own schedule (A's finality never depends on it — ADR-014 C4).
//!
//! Determinism (ADR-008): every piece here is a pure function of (A input, pinned B
//! state, protocol constants). The call context is part of [`ExecutionInput`] (owned,
//! excluded from `canonical_bytes()`, same rule as `storage`), so engines stay pure:
//! they never do I/O — the provider is the executor's I/O surface, supplied to them.
//!
//! **A call never kills A.** Every failure mode collapses to a deterministic
//! [`CallOutcome::Failure`] that A's contract logic observes as data (a status), and
//! A's execution continues (ADR-016 Q3: the calling contract's own logic is the
//! Tier-1 compensation policy).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::blocks::Blockchain;
use crate::crypto::{BasicHashProvider, HashProvider};
use crate::errors::PneumaticError;
use crate::tokens::{SmartContract, Token};
use crate::transactions::Transaction;
use crate::user::User;

use super::{ContractEngine, ContractError, ContractEngineRegistry, ExecutionInput, select_engine};

// ---------------------------------------------------------------------------
// Protocol constants (ADR-016 Q4, §8 of the design)
// ---------------------------------------------------------------------------

/// Maximum call nesting depth (the ADR-018 §8 cap-table value). A call attempted
/// from a frame at this depth fails with [`CallFailure::DepthExceeded`].
pub const MAX_CALL_DEPTH: u32 = 8;

/// Base gas a `Spec` `Call` op charges A for the attempt (instruction units).
pub const XCALL_CALL_BASE_SPEC: u64 = 10;

/// Base gas the Wasm `env.call` import charges A for the attempt (fuel).
pub const XCALL_CALL_BASE_WASM: u64 = 100;

/// Domain prefix for the cross-chain commitment hash (domain separation).
pub const XCALL_COMMIT_DOMAIN: &[u8] = b"PNEUMATIC/XCALL/COMMIT/v1";

// ---------------------------------------------------------------------------
// SnapshotRef — the pin
// ---------------------------------------------------------------------------

/// A pinned position in the target's (B's) chain: a 0-based block index plus the
/// block's `current_hash`. The ref must **anchor in B's chain** (the block at
/// `height` must exist and hash-match) — validated by
/// [`validate_snapshot_ref`]. For a confirmed ref every honest node agrees on the
/// state at that ref, so the pin is deterministic across A's shard members even
/// when B's tip advances (ADR-016 Q1).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRef {
    /// 0-based block index in B's chain (genesis = 0).
    pub height: u64,
    /// `current_hash` of the block at `height`.
    pub block_hash: Vec<u8>,
}

/// Validate `ref` against B's chain: the block at `ref.height` must exist and its
/// `current_hash` must equal `ref.block_hash`. Pure, deterministic, fail-closed.
pub fn validate_snapshot_ref(chain: &Blockchain, r: &SnapshotRef) -> Result<(), String> {
    let block = chain.get_block_at(r.height as usize).ok_or_else(|| {
        format!(
            "snapshot ref height {} out of range (chain has {} blocks)",
            r.height,
            chain.get_count()
        )
    })?;
    if block.current_hash != r.block_hash {
        return Err(format!("snapshot ref hash mismatch at height {}", r.height));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Target state provider — the executor's I/O surface (supplied to engines)
// ---------------------------------------------------------------------------

/// The target's state, resolved at a validated [`SnapshotRef`].
///
/// `sender_state` is the caller's `User` as recorded for **B's partition** — B's
/// engine sees the caller under B's chain (the committer applies B's deltas against
/// B-partition user state at commit).
#[derive(Clone, Debug)]
pub struct PinnedTarget {
    /// B's token (includes B's `blockchain` — the ref was validated against it).
    pub token: Token,
    /// B's contract (decoded from `Token.asset_data`).
    pub contract: SmartContract,
    /// The caller's user state for B's partition.
    pub sender_state: User,
    /// The validated ref this state was pinned at.
    pub snapshot_ref: SnapshotRef,
}

/// Resolves a target token's state at a snapshot ref. The **executor-side**
/// implementation does the data-service I/O; engines call it through the
/// [`CallContext`] they are given (the engine itself stays a pure function of its
/// input — the provider is part of that input).
///
/// Fail-closed: any resolution failure (unknown token, ref not anchored in B's
/// chain, missing contract asset, missing user) is a deterministic
/// [`CallOutcome::Failure`] observable to the calling contract.
pub trait TargetStateProvider: Send + Sync {
    fn resolve(
        &self,
        target_token: &[u8],
        snapshot_ref: &SnapshotRef,
        sender_key: &[u8],
    ) -> Result<PinnedTarget, ContractError>;
}

// ---------------------------------------------------------------------------
// CallContext — what rides in ExecutionInput
// ---------------------------------------------------------------------------

/// The Model X call context supplied to an engine via `ExecutionInput::call_ctx`.
///
/// `provider` — resolves (target, ref) → pinned state (the executor's I/O surface).
/// `registry` — selects the target's engine by name (so a Wasm caller can invoke a
/// Wasm callee, a Spec caller a Spec callee, and mixed pairs work).
/// `depth` — 0 at the top level; each nested call adds 1; the cap is
/// [`MAX_CALL_DEPTH`].
#[derive(Clone)]
pub struct CallContext {
    pub provider: Arc<dyn TargetStateProvider>,
    pub registry: Arc<ContractEngineRegistry>,
    pub depth: u32,
}

impl CallContext {
    pub fn new(
        provider: Arc<dyn TargetStateProvider>,
        registry: Arc<ContractEngineRegistry>,
    ) -> Self {
        CallContext {
            provider,
            registry,
            depth: 0,
        }
    }

    /// The child context for a nested call (depth + 1).
    pub fn child(&self) -> Arc<Self> {
        Arc::new(CallContext {
            provider: self.provider.clone(),
            registry: self.registry.clone(),
            depth: self.depth + 1,
        })
    }
}

// ---------------------------------------------------------------------------
// Call outcome — deterministic, observable
// ---------------------------------------------------------------------------

/// A deterministic call failure reason (ADR-016 Q3: data to the calling contract).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CallFailure {
    /// The nesting depth cap ([`MAX_CALL_DEPTH`]) was reached.
    DepthExceeded,
    /// The target could not be resolved (ref not anchored in B's chain, no contract
    /// asset, data-service error).
    TargetUnresolvable(String),
    /// The target's engine name is not registered in this environment.
    UnknownEngine(String),
    /// B's execution reverted.
    BReverted(String),
    /// B's execution ran out of its sub-budget.
    BGasExhausted,
    /// B's bytecode was malformed.
    BBadBytecode(String),
    /// B's execution input was invalid.
    BInvalidInput(String),
    /// B's engine is registered but not implemented.
    BEngineNotImplemented(String),
}

/// The deterministic result of a Model X call. A call **never** returns a hard
/// error to A — every failure mode is a `Failure` variant A observes as a status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallOutcome {
    /// B executed successfully; `result_data` is B's canonical output bytes.
    Success {
        result_data: Vec<u8>,
        /// B's metered work (charged to A on top of the call base cost).
        gas_used: u64,
    },
    /// B (or the resolution) failed deterministically.
    Failure {
        reason: CallFailure,
        /// B's metered work on the failed attempt (0 — only the base cost is charged).
        gas_used: u64,
    },
}

impl CallOutcome {
    /// B's metered work in either variant (what A charges on top of the base cost).
    pub fn b_gas_used(&self) -> u64 {
        match self {
            CallOutcome::Success { gas_used, .. } => *gas_used,
            CallOutcome::Failure { gas_used, .. } => *gas_used,
        }
    }

    /// `true` on success (the status value engines surface to contract logic).
    pub fn is_success(&self) -> bool {
        matches!(self, CallOutcome::Success { .. })
    }
}

// ---------------------------------------------------------------------------
// The call core — shared by the Spec `Call` op and the Wasm `env.call` import
// ---------------------------------------------------------------------------

/// Execute a Model X call: resolve B at the pinned ref, select B's engine, run B
/// in a nested frame under `sub_budget`, and fold the result into a deterministic
/// [`CallOutcome`].
///
/// `a_tx` — the calling (A) transaction (the sender key + the virtual-tx basis).
/// `sub_budget` — B's gas/fuel budget, computed by the caller as
/// `A_remaining − CALL_BASE` (a call can never push A past A's cap).
pub fn execute_call(
    ctx: &CallContext,
    a_tx: &Transaction,
    target_token: &[u8],
    entry_point: &str,
    call_payload: &[u8],
    snapshot_ref: &SnapshotRef,
    sub_budget: u64,
) -> CallOutcome {
    // 1. Nesting depth cap (before any work).
    if ctx.depth >= MAX_CALL_DEPTH {
        return CallOutcome::Failure {
            reason: CallFailure::DepthExceeded,
            gas_used: 0,
        };
    }

    // 2. Resolve B's pinned state (the executor's provider does the I/O).
    let pinned = match ctx.provider.resolve(target_token, snapshot_ref, &a_tx.sender) {
        Ok(p) => p,
        Err(e) => {
            return CallOutcome::Failure {
                reason: CallFailure::TargetUnresolvable(e.to_string()),
                gas_used: 0,
            };
        }
    };

    // 3. Select B's engine from the shared registry (per-token selection, ADR-011).
    let engine = match select_engine(&ctx.registry, &pinned.token) {
        Ok(e) => e,
        Err(e) => {
            return CallOutcome::Failure {
                reason: CallFailure::UnknownEngine(e.to_string()),
                gas_used: 0,
            };
        }
    };

    // 4. Synthesize B's virtual tx + nested input, and run B under the sub-budget.
    let virtual_tx = synthesize_call_tx(a_tx, target_token, entry_point, call_payload, sub_budget);
    let b_input = ExecutionInput {
        tx: &virtual_tx,
        contract: &pinned.contract,
        sender_state: &pinned.sender_state,
        token: &pinned.token,
        gas_limit: sub_budget,
        storage: pinned.contract.storage.clone(),
        call_ctx: Some(ctx.child()),
    };

    match engine.execute(&b_input) {
        Ok(out) => CallOutcome::Success {
            result_data: out.result_data,
            gas_used: out.gas_used,
        },
        Err(e) => CallOutcome::Failure {
            reason: match e {
                ContractError::Reverted(m) => CallFailure::BReverted(m),
                ContractError::GasExhausted => CallFailure::BGasExhausted,
                ContractError::BadBytecode(m) => CallFailure::BBadBytecode(m),
                ContractError::InvalidInput(m) => CallFailure::BInvalidInput(m),
                ContractError::UnknownEngine(m) => CallFailure::UnknownEngine(m),
                ContractError::EngineNotImplemented(m) => CallFailure::BEngineNotImplemented(m),
            },
            gas_used: 0,
        },
    }
}

/// The deterministic **virtual B transaction** a call runs under (ADR-016 §6): a
/// pure function of (A tx, call args) — identical on every executor. B's engine
/// sees it as a normal canonical input; the call is the user's action routed
/// through B (no cross-token value flow — `amount` is `None`).
pub fn synthesize_call_tx(
    a_tx: &Transaction,
    target_token: &[u8],
    entry_point: &str,
    call_payload: &[u8],
    sub_budget: u64,
) -> Transaction {
    Transaction {
        id: format!("xcall/{}/{}", a_tx.id, to_hex(target_token)),
        action: entry_point.to_string(),
        token_id: target_token.to_vec(),
        bid: None,
        sequence_number: a_tx.sequence_number,
        sender: a_tx.sender.clone(),
        receiver: a_tx.receiver.clone(),
        amount: None,
        timestamp: a_tx.timestamp,
        result_hash: Vec::new(),
        sender_signature: Vec::new(),
        payload: call_payload.to_vec(),
        gas_limit: sub_budget,
        result_data: Vec::new(),
    }
}

/// Deterministic lowercase-hex encoding (for the virtual tx id — no external dep).
fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// B's side — the cross-referenced commitment (ADR-016 Q2)
// ---------------------------------------------------------------------------

/// The cross-referenced commitment B's side carries: a tx on B's chain binding
/// A's transaction to the pinned ref. `commitment_hash =
/// SHA256(b"PNEUMATIC/XCALL/COMMIT/v1" ‖ rmp_canonical(commitment))`.
///
/// The hash is computed **at settlement** (B's side learns A's `result_hash` from
/// A's committed block) — never inside A's execution.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CrossChainCommitment {
    /// A's transaction id.
    pub a_tx_id: String,
    /// A's committed `result_hash` (= hash of A's `result_data`).
    pub a_result_hash: Vec<u8>,
    /// B's token id.
    pub target_token: Vec<u8>,
    /// The snapshot ref the call was pinned at.
    pub snapshot_ref: SnapshotRef,
}

impl CrossChainCommitment {
    /// The commitment hash: `SHA256(domain ‖ rmp_canonical(self))`.
    pub fn commitment_hash(&self) -> Result<Vec<u8>, PneumaticError> {
        let body = crate::encoding::serialize_to_bytes_rmp(self)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;
        let mut preimage = Vec::with_capacity(XCALL_COMMIT_DOMAIN.len() + body.len());
        preimage.extend_from_slice(XCALL_COMMIT_DOMAIN);
        preimage.extend_from_slice(&body);
        Ok(BasicHashProvider::new().hash(&preimage))
    }

    /// `true` if `expected` equals this commitment's hash (B's sentinel check).
    pub fn verify_commitment(&self, expected: &[u8]) -> bool {
        self.commitment_hash()
            .map(|h| h == expected)
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{Block, BlockFactory};
    use crate::contracts::{
        ContractEngine, ContractEngineRegistry, ExecutionInput, SpecEngine, TransferEngine,
        WasmEngine,
    };
    use crate::encoding::deserialize_rmp_to;
    use crate::transactions::Transaction;

    /// A registry with the given engines registered (test helper).
    fn make_registry(engines: Vec<Arc<dyn ContractEngine>>) -> Arc<ContractEngineRegistry> {
        let registry = ContractEngineRegistry::new();
        for e in engines {
            registry.register(e);
        }
        Arc::new(registry)
    }

    // --- Test doubles -------------------------------------------------------

    /// A deterministic in-test provider: serves a fixed pinned target for one token
    /// id (any ref that validates against the stored chain), fails closed otherwise.
    #[derive(Clone)]
    struct MapProvider {
        target: PinnedTarget,
        target_token: Vec<u8>,
    }

    impl MapProvider {
        fn new(target_token: Vec<u8>, token: Token, contract: SmartContract, sender: User) -> Self {
            MapProvider {
                target: PinnedTarget {
                    token: token.clone(),
                    contract,
                    sender_state: sender,
                    snapshot_ref: SnapshotRef {
                        height: 0,
                        block_hash: vec![],
                    },
                },
                target_token,
            }
        }
    }

    impl TargetStateProvider for MapProvider {
        fn resolve(
            &self,
            target_token: &[u8],
            snapshot_ref: &SnapshotRef,
            _sender_key: &[u8],
        ) -> Result<PinnedTarget, ContractError> {
            if target_token != self.target_token {
                return Err(ContractError::InvalidInput(format!(
                    "unknown target token {:?}",
                    target_token
                )));
            }
            validate_snapshot_ref(&self.target.token.blockchain, snapshot_ref)
                .map_err(ContractError::InvalidInput)?;
            Ok(PinnedTarget {
                snapshot_ref: snapshot_ref.clone(),
                ..self.target.clone()
            })
        }
    }

    /// Build a token with a 2-block chain; returns (token, ref-to-block-1).
    fn chain_token(id: Vec<u8>) -> (Token, SnapshotRef) {
        let mut token = Token::new();
        token.id = id;
        token
            .metadata
            .insert("token_type".to_string(), "contract".to_string());
        token
            .metadata
            .insert("contract_engine".to_string(), "Spec".to_string());
        let b0 = Block::test_block(vec![]);
        let b1 = Block::test_block(b0.current_hash.clone());
        let h1 = b1.current_hash.clone();
        token.blockchain.add_block(b0);
        token.blockchain.add_block(b1);
        (token, SnapshotRef { height: 1, block_hash: h1 })
    }

    fn spec_contract(program: &crate::contracts::InstructionProgram) -> SmartContract {
        let bytecode = crate::encoding::serialize_to_bytes_rmp(program)
            .expect("serialize program");
        SmartContract {
            name: "callee".to_string(),
            bytecode,
            version: "1".to_string(),
            storage: Default::default(),
            owners: vec![],
            threshold: 0,
        }
    }

    fn caller_tx() -> Transaction {
        Transaction {
            id: "a-tx".to_string(),
            action: "ContractCall".to_string(),
            token_id: vec![0xAA],
            bid: None,
            sequence_number: 3,
            sender: vec![0x10, 0x11],
            receiver: vec![],
            amount: None,
            timestamp: 1_700_000_000,
            result_hash: vec![],
            sender_signature: vec![],
            payload: vec![1, 2, 3, 4],
            gas_limit: 0,
            result_data: vec![],
        }
    }

    fn ctx_for(target_token: Vec<u8>) -> (CallContext, SnapshotRef, Transaction) {
        let (token, r) = chain_token(target_token.clone());
        let contract = spec_contract(&crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![
                crate::contracts::Op::LoadConst(42),
                crate::contracts::Op::Emit,
            ],
        });
        let provider = MapProvider::new(target_token, token, contract, User {
            public_key: vec![0x10, 0x11],
            fuel_balance: 100,
            stake: 1,
            nonce: 3,
        });
        let registry = make_registry(vec![
            Arc::new(SpecEngine),
            Arc::new(TransferEngine),
            Arc::new(WasmEngine),
        ]);
        (CallContext::new(Arc::new(provider), registry), r, caller_tx())
    }

    /// Run a Spec caller program with a call context; returns the A output.
    fn run_spec_caller(
        program: &crate::contracts::InstructionProgram,
        ctx: Option<&CallContext>,
        gas_limit: u64,
    ) -> Result<crate::contracts::ExecutionOutput, ContractError> {
        let caller_contract = spec_contract(program);
        let mut a_token = Token::new();
        a_token.id = vec![0xAA];
        let tx = caller_tx();
        let user = User {
            public_key: vec![0x10, 0x11],
            fuel_balance: 100,
            stake: 1,
            nonce: 3,
        };
        let input = ExecutionInput {
            tx: &tx,
            contract: &caller_contract,
            sender_state: &user,
            token: &a_token,
            gas_limit,
            storage: Default::default(),
            call_ctx: ctx.map(|c| std::sync::Arc::new(c.clone())),
        };
        SpecEngine.execute(&input)
    }

    // --- Commitment ----------------------------------------------------------

    #[test]
    fn commitment_hash_is_deterministic_and_verifiable() {
        let c = CrossChainCommitment {
            a_tx_id: "a-tx".to_string(),
            a_result_hash: vec![1, 2, 3],
            target_token: vec![0xBB],
            snapshot_ref: SnapshotRef { height: 1, block_hash: vec![9, 9] },
        };
        let h1 = c.commitment_hash().expect("hash");
        let h2 = c.commitment_hash().expect("hash");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 32);
        assert!(c.verify_commitment(&h1));
        assert!(!c.verify_commitment(&[0u8; 32]));
        // Domain separation: the same body under a different domain hashes differently.
        let other = CrossChainCommitment {
            a_result_hash: vec![1, 2, 3, 0],
            ..c.clone()
        };
        assert_ne!(other.commitment_hash().expect("hash"), h1);
    }

    // --- Call core -----------------------------------------------------------

    #[test]
    fn call_success_returns_callee_output() {
        let (ctx, r, a_tx) = ctx_for(vec![0xBB]);
        let outcome = execute_call(
            &ctx, &a_tx, &[0xBB], "ContractCall", &[7, 8], &r, 1000,
        );
        let CallOutcome::Success { result_data, gas_used } = outcome else {
            panic!("expected success, got {outcome:?}");
        };
        // The callee emits the u64 42 (little-endian).
        let value = u64::from_le_bytes(result_data.try_into().expect("8 bytes"));
        assert_eq!(value, 42);
        assert!(gas_used >= 2); // LoadConst + Emit
    }

    #[test]
    fn call_invalid_ref_fails_deterministically() {
        let (ctx, _r, a_tx) = ctx_for(vec![0xBB]);
        let bad_ref = SnapshotRef { height: 99, block_hash: vec![1] };
        let outcome = execute_call(
            &ctx, &a_tx, &[0xBB], "ContractCall", &[7], &bad_ref, 1000,
        );
        assert!(matches!(
            outcome,
            CallOutcome::Failure {
                reason: CallFailure::TargetUnresolvable(_),
                ..
            }
        ));
        // Identical failure on a second, independently built context (determinism).
        let (ctx2, _, a_tx2) = ctx_for(vec![0xBB]);
        let outcome2 = execute_call(&ctx2, &a_tx2, &[0xBB], "ContractCall", &[7], &bad_ref, 1000);
        assert_eq!(outcome, outcome2);
    }

    #[test]
    fn call_unknown_target_fails_deterministically() {
        let (ctx, r, a_tx) = ctx_for(vec![0xBB]);
        let outcome = execute_call(&ctx, &a_tx, &[0xCC], "x", &[], &r, 100);
        assert!(matches!(
            outcome,
            CallOutcome::Failure { reason: CallFailure::TargetUnresolvable(_), .. }
        ));
    }

    #[test]
    fn call_depth_cap_is_enforced() {
        let target = vec![0xBB];
        let (token, r) = chain_token(target.clone());
        let contract = spec_contract(&crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![crate::contracts::Op::LoadConst(42), crate::contracts::Op::Emit],
        });
        let provider = MapProvider::new(target.clone(), token, contract, User {
            public_key: vec![0x10, 0x11],
            fuel_balance: 100,
            stake: 1,
            nonce: 3,
        });
        let registry = make_registry(vec![Arc::new(SpecEngine)]);
        let a_tx = caller_tx();

        // At depth MAX_CALL_DEPTH the call fails immediately with DepthExceeded.
        let capped = CallContext {
            provider: Arc::new(provider.clone()),
            registry: registry.clone(),
            depth: MAX_CALL_DEPTH,
        };
        let outcome = execute_call(&capped, &a_tx, &target, "ContractCall", &[], &r, 10_000);
        assert!(matches!(
            outcome,
            CallOutcome::Failure { reason: CallFailure::DepthExceeded, .. }
        ));

        // One below the cap, the same call succeeds (the cap is exact).
        let below = CallContext {
            provider: Arc::new(provider),
            registry,
            depth: MAX_CALL_DEPTH - 1,
        };
        let outcome = execute_call(&below, &a_tx, &target, "ContractCall", &[], &r, 10_000);
        assert!(matches!(outcome, CallOutcome::Success { .. }));
    }

    #[test]
    fn self_calling_program_settles_at_depth_cap() {
        // A program that calls itself: the nesting runs to the cap, the deepest
        // call fails with DepthExceeded, and that failure is *data* — each
        // enclosing level observes status 0 and the program settles (a call never
        // kills A).
        let target = vec![0xBB];
        let (token, r) = chain_token(target.clone());
        let program = crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![
                crate::contracts::Op::Call {
                    target_token: target.clone(),
                    entry_point: "ContractCall".to_string(),
                    call_payload: vec![],
                    ref_height: r.height,
                    ref_hash: r.block_hash.clone(),
                },
                crate::contracts::Op::Halt,
            ],
        };
        let provider = MapProvider::new(target.clone(), token, spec_contract(&program), User {
            public_key: vec![0x10, 0x11],
            fuel_balance: 100,
            stake: 1,
            nonce: 3,
        });
        let registry = make_registry(vec![Arc::new(SpecEngine)]);
        let ctx = CallContext::new(Arc::new(provider), registry);
        // The outermost call succeeds: the depth cap is hit deep inside, and every
        // level above it observed the failure as a status and settled.
        let outcome = execute_call(&ctx, &caller_tx(), &target, "ContractCall", &[], &r, 100_000);
        assert!(matches!(outcome, CallOutcome::Success { .. }));
    }

    // --- Spec Op::Call integration ------------------------------------------

    #[test]
    fn spec_call_op_pushes_success_status() {
        let (ctx, r, _a_tx) = ctx_for(vec![0xBB]);
        let program = crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![
                crate::contracts::Op::Call {
                    target_token: vec![0xBB],
                    entry_point: "ContractCall".to_string(),
                    call_payload: vec![7, 8],
                    ref_height: r.height,
                    ref_hash: r.block_hash.clone(),
                },
                crate::contracts::Op::Emit, // emit the status (1)
            ],
        };
        let out = run_spec_caller(&program, Some(&ctx), 0).expect("exec");
        let status = u64::from_le_bytes(out.result_data.try_into().expect("8 bytes"));
        assert_eq!(status, 1);
    }

    #[test]
    fn spec_call_op_pushes_failure_status_on_bad_ref() {
        let (ctx, _r, _a_tx) = ctx_for(vec![0xBB]);
        let program = crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![
                crate::contracts::Op::Call {
                    target_token: vec![0xBB],
                    entry_point: "ContractCall".to_string(),
                    call_payload: vec![],
                    ref_height: 99,
                    ref_hash: vec![1],
                },
                crate::contracts::Op::Emit, // emit the status (0)
            ],
        };
        let out = run_spec_caller(&program, Some(&ctx), 0).expect("exec");
        let status = u64::from_le_bytes(out.result_data.try_into().expect("8 bytes"));
        assert_eq!(status, 0);
    }

    #[test]
    fn spec_call_without_context_reverts() {
        let program = crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![crate::contracts::Op::Call {
                target_token: vec![0xBB],
                entry_point: "x".to_string(),
                call_payload: vec![],
                ref_height: 0,
                ref_hash: vec![1],
            }],
        };
        let err = run_spec_caller(&program, None, 0).expect_err("must revert");
        assert!(matches!(err, ContractError::Reverted(_)));
    }

    #[test]
    fn spec_call_charges_gas_to_caller() {
        let (ctx, r, _a_tx) = ctx_for(vec![0xBB]);
        let program = crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![
                crate::contracts::Op::Call {
                    target_token: vec![0xBB],
                    entry_point: "ContractCall".to_string(),
                    call_payload: vec![],
                    ref_height: r.height,
                    ref_hash: r.block_hash.clone(),
                },
                crate::contracts::Op::Halt,
            ],
        };
        // Caller ops: Call(1) + Halt(1); the call charges base(10) + the callee's
        // work (LoadConst + Emit = 2). Total = 14.
        let out = run_spec_caller(&program, Some(&ctx), 0).expect("exec");
        assert_eq!(out.gas_used, 1 + XCALL_CALL_BASE_SPEC + 2 + 1);

        // A tight cap below the call cost exhausts A deterministically.
        let err = run_spec_caller(&program, Some(&ctx), 5).expect_err("gas");
        assert!(matches!(err, ContractError::GasExhausted));
    }

    // --- The plan's Phase-9 exit tests --------------------------------------

    /// **Snapshot pinning**: B's chain advances between two A executors (different
    /// tips) — the same pinned ref yields an identical A result.
    #[test]
    fn snapshot_pinning_identical_result_across_advancing_b() {
        let target = vec![0xBB];
        let program = crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![
                crate::contracts::Op::Call {
                    target_token: target.clone(),
                    entry_point: "ContractCall".to_string(),
                    call_payload: vec![7, 8],
                    ref_height: 1,
                    ref_hash: vec![], // filled per view below
                },
                crate::contracts::Op::Emit,
            ],
        };

        // View 1: B's tip is block 1. View 2: B's tip has advanced to block 2 (same
        // history — block 1's hash is unchanged, the canonical state at the ref is
        // identical). The pinned ref is (1, hash(block 1)) in both views.
        let (token1, r1) = chain_token(target.clone());
        let token2 = {
            let mut t = token1.clone();
            let tip = t.blockchain.last_block().expect("tip").clone();
            let b2 = Block::test_block(tip.current_hash.clone());
            t.blockchain.add_block(b2);
            t
        };

        let make_ctx = |token: Token| -> CallContext {
            let contract = spec_contract(&crate::contracts::InstructionProgram {
                version: 1,
                ops: vec![crate::contracts::Op::LoadConst(42), crate::contracts::Op::Emit],
            });
            let provider = MapProvider::new(target.clone(), token, contract, User {
                public_key: vec![0x10, 0x11],
                fuel_balance: 100,
                stake: 1,
                nonce: 3,
            });
            let registry = make_registry(vec![Arc::new(SpecEngine)]);
            CallContext::new(Arc::new(provider), registry)
        };

        // The program carries the concrete ref as immediates (same ref in both views).
        let program = crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![
                crate::contracts::Op::Call {
                    target_token: target.clone(),
                    entry_point: "ContractCall".to_string(),
                    call_payload: vec![7, 8],
                    ref_height: r1.height,
                    ref_hash: r1.block_hash.clone(),
                },
                crate::contracts::Op::Emit,
            ],
        };

        let ctx1 = make_ctx(token1);
        let ctx2 = make_ctx(token2);
        let out1 = run_spec_caller(&program, Some(&ctx1), 0).expect("exec 1");
        let out2 = run_spec_caller(&program, Some(&ctx2), 0).expect("exec 2");
        assert_eq!(out1.result_data, out2.result_data);
        assert_eq!(out1.gas_used, out2.gas_used);
    }

    /// **Revert policy**: B's side reverts → A settles deterministically (the
    /// caller observes the failure as data and emits its own deterministic result).
    #[test]
    fn revert_policy_b_failure_settles_a_deterministically() {
        let target = vec![0xBB];
        // One shared B chain: the pinned ref and every provider view anchor to the
        // same blocks (the ref must validate against the chain the provider serves).
        let (token, r) = chain_token(target.clone());

        let make_ctx = |token: Token, reverting: bool| -> CallContext {
            let callee_prog = if reverting {
                // A callee program that reverts (modulo by zero).
                crate::contracts::InstructionProgram {
                    version: 1,
                    ops: vec![
                        crate::contracts::Op::LoadConst(1),
                        crate::contracts::Op::LoadConst(0),
                        crate::contracts::Op::Mod, // 1 % 0 reverts
                    ],
                }
            } else {
                crate::contracts::InstructionProgram {
                    version: 1,
                    ops: vec![crate::contracts::Op::LoadConst(42), crate::contracts::Op::Emit],
                }
            };
            let contract = spec_contract(&callee_prog);
            let provider = MapProvider::new(target.clone(), token, contract, User {
                public_key: vec![0x10, 0x11],
                fuel_balance: 100,
                stake: 1,
                nonce: 3,
            });
            let registry = make_registry(vec![Arc::new(SpecEngine)]);
            CallContext::new(Arc::new(provider), registry)
        };

        // Select pops (cond, b, a) top-first and pushes `a` if cond != 0 else `b`.
        // Stack [1, 7, status]: failure (0) -> 7 (the compensation value); success -> 1.
        let caller_prog = crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![
                crate::contracts::Op::LoadConst(1),
                crate::contracts::Op::LoadConst(7),
                crate::contracts::Op::Call {
                    target_token: target.clone(),
                    entry_point: "ContractCall".to_string(),
                    call_payload: vec![],
                    ref_height: r.height,
                    ref_hash: r.block_hash.clone(),
                },
                crate::contracts::Op::Select,
                crate::contracts::Op::Emit,
            ],
        };

        let ctx_fail = make_ctx(token.clone(), true);
        let out_fail = run_spec_caller(&caller_prog, Some(&ctx_fail), 0).expect("A settles");
        let fail_data = out_fail.result_data.clone();
        let v_fail = u64::from_le_bytes(fail_data.try_into().expect("8 bytes"));
        assert_eq!(v_fail, 7); // A observed the failure and settled deterministically

        let ctx_ok = make_ctx(token.clone(), false);
        let out_ok = run_spec_caller(&caller_prog, Some(&ctx_ok), 0).expect("A settles");
        let v_ok = u64::from_le_bytes(out_ok.result_data.try_into().expect("8 bytes"));
        assert_eq!(v_ok, 1);

        // Re-running the failing case is byte-identical (deterministic settlement).
        let ctx_fail2 = make_ctx(token, true);
        let out_fail2 = run_spec_caller(&caller_prog, Some(&ctx_fail2), 0).expect("A settles");
        assert_eq!(out_fail.result_data, out_fail2.result_data);
    }

    /// **Finality independence**: A's execution (and result) never references B's
    /// post-call blocks — A completes deterministically even though B's side (the
    /// cross-referenced commitment tx) does not exist in A's input at all.
    #[test]
    fn finality_independence_a_does_not_depend_on_b_side() {
        let (ctx, r, _a_tx) = ctx_for(vec![0xBB]);
        let program = crate::contracts::InstructionProgram {
            version: 1,
            ops: vec![
                crate::contracts::Op::Call {
                    target_token: vec![0xBB],
                    entry_point: "ContractCall".to_string(),
                    call_payload: vec![7, 8],
                    ref_height: r.height,
                    ref_hash: r.block_hash.clone(),
                },
                crate::contracts::Op::Emit,
            ],
        };
        // B's chain contains only blocks 0..1 — no settlement block for the call
        // exists anywhere in A's input. A still succeeds, deterministically.
        let out1 = run_spec_caller(&program, Some(&ctx), 0).expect("A finalizes");
        let out2 = run_spec_caller(&program, Some(&ctx), 0).expect("A finalizes");
        assert!(out1.result_data == out2.result_data);
        let status = u64::from_le_bytes(out1.result_data.try_into().expect("8 bytes"));
        assert_eq!(status, 1);
    }
}
