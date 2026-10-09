use std::io;

use pneumatic_core::tokens::BlockCommitError;

/// Helper: convert bytes to lowercase hex string.
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Errors specific to the Committer crate.
#[derive(Debug)]
pub enum CommitterError {
    /// Serialization/deserialization failure
    Encoding(io::Error),
    /// Failed to deserialize a message body
    Deserialization(io::Error),
    /// Token not found in local cache
    TokenNotFound(String),
    /// Transaction not found in pending registry
    TransactionNotFound(String),
    /// Transaction is not in the expected Finalizing state
    TransactionNotInFinalizing(String),
    /// The incoming commit's transaction payload differs from the validated/pooled transaction
    /// (AUDIT Phase 3.5 / H12): the block on the wire embeds a transaction that is not the one the
    /// pipeline validated, so the commit is rejected and never appended.
    TransactionPayloadMismatch(String),
    /// Commit message env_id does not match this committer's environment
    EnvironmentMismatch { expected: String, got: String },
    /// Proposed block has an empty hash
    InvalidBlockHash,
    /// The block's finalizer signature is missing or does not verify under the key the
    /// verifier expected — fail closed (AUDIT Phase 3.3 / C5).
    InvalidFinalizerSignature,
    /// The finalizer signature is a valid signature, from the right key, over a hash —
    /// just not over the block in front of us. The digest is reconstructed from the
    /// received bytes and compared with the hash the signature claims; a difference means
    /// the embedded transaction, its execution result, or its executor signatures were
    /// changed after signing.
    ///
    /// This is the check that made the others mean anything. Verifying a signature
    /// against `finalizer_sig.transaction_hash` — the hash shipped inside the same message
    /// as the bytes it is supposed to authenticate — proves a hash was signed and nothing
    /// about the payload. It is also what has to exist before the commit path can accept
    /// a block whose execution result differs from this node's own local execution (the
    /// committer's H12 payload compare was the only thing catching a rewritten result,
    /// and it is the compare that keeps the mesh from converging — see
    /// `fact-h12-assumes-lockstep-execution`).
    SignaturePayloadMismatch(String),
    /// Block validation/commit failed on the token
    BlockCommit(BlockCommitError),
    /// Unknown message action
    UnknownAction(String),
    /// Sender's Ed25519 public key (hex) is not a registered node — the
    /// committer cannot authenticate the envelope's origin.
    UnauthenticatedSender(String),
    /// Sender is registered but its role is not permitted to send this
    /// action: `<public_key hex>: action=<action> role=<role>`.
    UnauthorizedRole(String),
    /// Underlying PneumaticError from core
    Core(pneumatic_core::errors::PneumaticError),
    /// Internal serialization failure (gossip protocol)
    InternalSerialization,
    /// Gas deduction could not be completed: `get_user`/`save_user` returned a
    /// `DataError`. The committed block stands (it was validated/finalized), but the
    /// sender's fuel balance was not debited, so the transaction does not reach the
    /// `Committed` state — the failure is surfaced rather than silently swallowed
    /// (AUDIT Phase 4.5 / M11: "cannot silently free gas or overdraw").
    GasDeduction {
        sender: String,
        tx_id: String,
        gas_used: u64,
        cause: String,
    },
    /// A committed block lost its conflict resolution and was discarded on the commit
    /// path (AUDIT Phase 5.2 / H2): the incoming block did not win its conflict at the
    /// tip, so it is rejected and the existing chain state is preserved.
    LoserDiscarded,
    /// The snapshot for an epoch could not be persisted (or was rejected on a
    /// prior load) — `save_stake_snapshot`/`save_executor_set` returned a
    /// `DataError`, or the SHA-256 envelope failed its integrity check. The epoch
    /// advance is aborted rather than silently proceeding with a snapshot that
    /// may be stale or missing (AUDIT Phase 5.4 / H9/M8). `kind` is `"stake"` or
    /// `"executor"`.
    SnapshotPersist {
        epoch: u64,
        kind: &'static str,
        cause: String,
    },
    /// A token distribution carried an id already present in the local token cache
    /// (AUDIT Phase 5.5 / H13): a peer may not replace or swap in the chain or
    /// metadata of a token that already exists. The rejected distribution is
    /// discarded and the cached token is left intact. `token_id` is the hex id.
    TokenConflict(String),
    /// S5.3: the committer's authoritative shielded re-check (step 0 of
    /// `ShieldedPool::apply_update`, `ShieldedValidationSpec::validate_shielded`
    /// against the pool's OWN state) rejected the proof. Soundness does not
    /// depend on the sentinel/finalizer verdicts — a tx with a full finalizer
    /// quorum still fails here if its proof does not verify (S5.3
    /// discriminator 5). `cause` is the validation reason summary.
    ShieldedProofInvalid {
        tx_id: String,
        cause: String,
    },
    /// S5.3: a shielded-pool state fault. `kind` distinguishes:
    /// `"corrupt"` (envelope fingerprint mismatch — `SnapshotCorrupt`),
    /// `"integrity"` (rebuilt state contradicts itself, e.g.
    /// `current_root() != tree.root()`, a bad Fp encoding, a leaf-count
    /// mismatch, or a recorded `post_root` history inconsistent with the leaf
    /// sequence), `"invalid_root"` (a persisted 32-byte root is not a valid
    /// `Fp`). `corrupt`/`integrity` are fail-closed boot/apply errors — the
    /// committer refuses to run on a pool it cannot reconstruct exactly
    /// (never silently re-seed: that forgets prior spends).
    PoolState {
        kind: &'static str,
        cause: String,
    },
    /// S5.3: persisting the pool state failed (roadmap 2.5 durability) —
    /// `save_shielded_pool` returned a `DataError`. The commit is rolled
    /// back / the finalization surfaced; a block is never reported committed
    /// without its durable nullifier record.
    PoolPersist {
        cause: String,
    },
    /// S5.3: a rollback was requested for a block with no recorded pool delta
    /// (no shielded payload, or the delta was already reverted) — never a
    /// silent skip. `block` is the hex block hash.
    PoolRollback {
        block: String,
    },
    /// Phase 1 (ingress roadmap): persisting the ADVANCED token after a
    /// successful chain append failed — `save_token` returned a `DataError`.
    /// The block is on this committer's chain but the shared store has not
    /// received the new tip, so the committed block is not observable through
    /// the data service. Surfaced, never swallowed: a commit that cannot
    /// report its persistence is not "committed" for anyone downstream.
    TokenPersist {
        token_id: String,
        cause: String,
    },
    /// ADR-004: this block did not come from the Finalizer the Sentinel routed its
    /// transaction to, or carried no routing decision at all, or its attestation
    /// does not hold. The Committer is the node that decides what enters the chain,
    /// so this is the check that actually prevents a fork from becoming history:
    /// with N hosts finalizing one transaction, the one that commits first wins and
    /// the rest are divergent copies (`fact-composite-per-host-fork-divergence`).
    ///
    /// Distinct from `InvalidFinalizerSignature` (the signature is bad) and from
    /// `UnauthorizedRole` (the sender is not a Finalizer at all): here the sender may
    /// be a perfectly well-behaved Finalizer that was simply not the one chosen, and
    /// an operator needs to tell "our routing is broken" apart from "someone is
    /// attacking us".
    Routing(String),
}

impl From<io::Error> for CommitterError {
    fn from(err: io::Error) -> Self {
        CommitterError::Encoding(err)
    }
}

impl From<pneumatic_core::errors::PneumaticError> for CommitterError {
    fn from(err: pneumatic_core::errors::PneumaticError) -> Self {
        CommitterError::Core(err)
    }
}

impl From<pneumatic_core::tokens::BlockCommitError> for CommitterError {
    fn from(err: pneumatic_core::tokens::BlockCommitError) -> Self {
        CommitterError::BlockCommit(err)
    }
}

