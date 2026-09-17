# Phase S4.1 Implementation Plan — `ShieldedValidationSpec`

> Expanded, actionable implementation plan for **Phase S4.1** of
> [`pneumatic-shielded-implementation-plan.md`](pneumatic-shielded-implementation-plan.md).
>
> Scope: the *validation spec* only. S4.1 defines **what a shielded transaction
> must satisfy** and wires the four fail-closed checks. The data structures the
> nullifier check and merkle-root check *read from* (S4.2 `NullifierRegistry`,
> S4.3 root history) are **interfaces defined here and implemented by S4.2/S4.3**
> — see *Dependency model* below. The pipeline wiring that *invokes* the spec
> (S5.1 sentinel branch, S5.2 finalizer re-verify) is out of scope but called
> out where S4.1 must leave a seam.

---

## 1. Context & what S4.1 delivers

S4.1 implements `ShieldedValidationSpec` (name `"Shielded"`), the spec the
Sentinel runs as an **advisory pre-check** on a `ShieldedTransaction` before
forwarding it to finalizers (S5.1 step 2). It is *not* the consensus gate — the
authoritative, guarded re-check is the Committer's apply (S5.3). Its job:
reject obvious garbage (bad proof, already-spent nullifier, stale root, malformed
fields) before any finalizer work.

Four checks, **in order, all fail-closed** (each short-circuits — the first
failing check wins; see *Discriminator: check ordering*):

| # | Check | Fails closed as |
|---|---|---|
| 1 | **Structural** — nullifier/commitment counts non-empty and ≤ circuit capacity, `merkle_root` decodes, commitments decode to valid curve points, nullifiers pairwise distinct within the tx | `InvalidCommitment` |
| 2 | **Nullifier set** — each nullifier is not already spent (double-spend; the nullifier set's job, S4.2) | `StaleNullifier` |
| 3 | **Merkle-root freshness** — `merkle_root` == pool's current root OR within the last-K committed roots (S4.3 K-recency window) | `StaleMerkleRoot` |
| 4 | **Proof** — `verify_shielded_proof` (S2.2) accepts the proof over the tx's public inputs | `InvalidShieldedProof` |

Plus `calculate_risk`-equivalent: shielded txs report a **fixed, neutral** risk
(`affected_parties: 2, amount: 0`) — the amount is unknown by design. The
environment's `max_risk` gate still applies to that fixed value.

### Deliverables
- New `ValidationFailureReason` variants (`errors.rs`, additive).
- New `ShieldedValidationSpec` + `register_shielded()` seam (`validation.rs`).
- New `TransactionValidationSpec` method `validate_shielded` with a **default
  fail-closed** impl.
- `PublicInputs` reconstruction from a `ShieldedTransaction` (`shielded/`).
- The two dependency interfaces the spec reads from: `NullifierMembership` and
  `MerkleRootHistory` (implemented by S4.2 / S4.3).

### Ground rules applied
- **Additive only** (`Transaction` unchanged; `SignedTransaction.shielded`
  already exists — S3.1). The ONE wire primitive change is an additive
  `ShieldedTransaction.spent_commitments` field (see *Decision 3*), with a
  wire-compat note appended at the end.
- **Fail closed**: missing/unknown spec, nullifier state, root, or proof input is
  an `Err`, never a silent accept.
- **Existing specs untouched**: `SelfSigned`/`Executed` and `register_defaults()`
  are byte-identical. The new spec is registered **explicitly**, opt-in.

---

## 2. Grounding — APIs S4.1 reuses (all verified in-tree)

| Surface | Signature (verified) | Used for |
|---|---|---|
| `TransactionValidationSpec` | `validate(&self,&Transaction,&Token,&Env)`, `calculate_risk(&self,&Transaction)`, `name(&self)->&str` (`validation.rs:16-31`) | the spec implements this; a NEW `validate_shielded` method is added with a default impl |
| `ValidationSpecRegistry` | `register`/`get`/`register_defaults` (`validation.rs:292-316`) | register `"Shielded"` via a **new** `register_shielded()` — *not* in `register_defaults` |
| `ValidationFailureReason` | enum (`errors.rs:103-148`) | add `InvalidShieldedProof`, `StaleNullifier`, `StaleMerkleRoot`, `InvalidCommitment` (+ two reserved, *Decision 4*) |
| `ShieldedTransaction` | fields `transactions.rs:362-387`; `canonical_bytes()` `:399`; `hash()` `:408` | the value validated |
| `TransactionValidationResult` | `valid(k,risk)` / `invalid(reasons)`; `is_valid`/`risk`/`failure_reasons` (`transactions.rs:209-243`) | the spec's return type |
| `TransactionRiskFactor` | `score()` 0.0-1.0 (`errors.rs:166`) | fixed neutral risk → gate on `env.max_risk` |
| `ShieldedVerifier` | `new(&ActionCircuit,k)->Result` `verify.rs:111`; `verify(&proof,&PublicInputs)->Result` `:163`; `instances_for(&PublicInputs)` `:144` | **check 4** |
| `PublicInputs` | `{ nullifier, commit_x, commit_y, merkle_root, output_commit_x, output_commit_y, fee }` (`circuit.rs:380-395`) | reconstructed from the tx → verifier inputs |
| `ActionCircuit` | `new` `circuit.rs:308`; `without_witnesses` `:408` | template to build the VK once (`keygen_vk` only needs circuit *structure*, not witness data) |
| `bytes_to_root` / `root_to_bytes` | `bytes_to_root(&[u8;32])->Option<Fp>` (fallible) (`tree.rs`, re-exported `mod.rs:52`) | `merkle_root` wire ↔ circuit `Fp`; `None` → fail closed |
| `commit` | `commit(&ShieldedNote)->EpAffine` (`note.rs:99`) | test fixtures / (re)compute a leaf |
| `PneumaticError::Shielded` | variant (`errors.rs:47`) | a `verify` `Err` surfaces here; distinct from `Validation(..)` |
| Sentinel `TransactionValidator` | `validate_transaction(&Transaction,&Message)` (`sentinel/src/transaction_validator.rs:39`) | S5.1 will look up `"Shielded"` here and call `spec.validate_shielded(...)` instead of `spec.validate` |

### Dependency status (already built → S4.1 builds on)
- `ShieldedTransaction` — DONE (S3.1).
- `ShieldedVerifier` + `PublicInputs` + `ActionCircuit` — DONE (S2.1/S2.2).
- `halo2_proofs = "=0.3.5"` — **production dependency** (`Cargo.toml:71`),
  promoted from dev-dep when the circuit moved into non-test code. `pub mod
  shielded;` is production (`lib.rs:27`). → **S4.1 calls `ShieldedVerifier::verify`
  directly from production code; no dependency-promotion step is needed.** (The
  `shielded/mod.rs:8-13` comment saying halo2 is dev-only is stale/scheduled.)

---

## 3. Design decisions (the meat of S4.1)

### Decision 1 — How a `&ShieldedTransaction` reaches the spec: a trait method with a default impl

The trait takes `&Transaction`; the shielded path takes `&ShieldedTransaction`.
The sentinel dispatches *by spec name* then calls `spec.validate(tx, …)`
(`transaction_validator.rs:81`). The clean, adapter-free resolution: **add a
new optional method `validate_shielded` to `TransactionValidationSpec` with a
default impl that fails closed.**

```rust
impl TransactionValidationSpec for ShieldedValidationSpec {
    // inherited `validate(&Transaction,…)` is never hit for shielded txs:
    fn validate(&self, _tx: &Transaction, _token: &Token, _env: &EnvironmentMetadata)
        -> Result<TransactionValidationResult, PneumaticError> {
        // Fail closed: the shielded spec validates a ShieldedTransaction, not a
        // plain Transaction. A plain-Tx reaching this spec is a wiring bug.
        Err(PneumaticError::Validation(vec![ValidationFailureReason::UnsupportedAction]))
    }

    // NEW — the shielded path.
    fn validate_shielded(
        &self,
        tx: &ShieldedTransaction,
        env_data: &EnvironmentMetadata,
        deps: &ShieldedValidationDeps,   // check 2 + check 3 inputs
    ) -> Result<TransactionValidationResult, PneumaticError> {
        // checks 1→2→3→4, short-circuiting; see S4.1.3–S4.1.5
    }

    fn name(&self) -> &str { "Shielded" }
}

// Default impl on the TRAIT — any spec that does NOT override fails closed.
impl TransactionValidationSpec for <other specs> { /* inherited */ }
```

Default impl (placed on the trait body so `SelfSigned`/`Executed` inherit it):
```rust
fn validate_shielded(&self, _tx: &ShieldedTransaction, _env: &EnvironmentMetadata,
                     _deps: &ShieldedValidationDeps) -> Result<TransactionValidationResult, PneumaticError> {
    // A spec that is not ShieldedValidationSpec cannot validate a shielded tx.
    Err(PneumaticError::Validation(vec![ValidationFailureReason::UnsupportedAction]))
}
```

Why this and not an adapter/`match` on the tx type:
- The sentinel's shielded branch (S5.1) looks up the spec by `"Shielded"` and
  calls `spec.validate_shielded(tx, env, deps)`. No type-`match` needed.
- **Fail closed by construction**: a `"ShieldedTransfer"` reaching any spec that
  isn't `ShieldedValidationSpec` errors (mirrors Phase 3.2's
  reject-unknown-validator). And if no `"Shielded"` spec is registered, the
  sentinel lookup returns `None` → `UnsupportedAction`.
- `SelfSigned`/`Executed` and `register_defaults()` are **untouched** — they
  inherit the default.

### Decision 2 — Dependency model: the spec reads from interfaces, not concrete types

Checks 2 and 3 need pool state that S4.2 (nullifier set) and S4.3 (root history)
build *after* S4.1. To keep S4.1 buildable and testable on its own, the spec
depends on two **tiny read-only traits**, implemented with in-memory fakes in
tests and by the concrete S4.2/S4.3 structures at runtime:

```rust
/// Check 2: "has this nullifier already been spent?"
pub trait NullifierMembership {
    fn contains_nullifier(&self, nullifier: [u8; 32]) -> bool;
}

/// Check 3: "is this root fresh?" — roots ordered oldest→newest (last = current).
pub struct RootSnapshot { pub root: [u8; 32], pub height: u64 }
pub trait MerkleRootHistory {
    fn root_history(&self) -> &[RootSnapshot];
}

/// The deps bundle the spec is handed at call time (stateless spec).
pub struct ShieldedValidationDeps<'a> {
    pub spent: &'a dyn NullifierMembership,
    pub roots: &'a dyn MerkleRootHistory,
    /// K-recency window from the environment; default small (S4.3).
    pub recency_window: usize,
}
```

Runtime binding:
- **Composite node** (primary; `node-server` hosts all roles): the sentinel and
  finalizer share one `Arc<ShieldedPool>` (S5.3) that implements both traits; the
  spec reads it read-only.
- **Split deployments**: the pool update is a pure function of committed block
  history (S3.1 — hash-bound inside `signed_trans.shielded`), so a
  sentinel/finalizer rebuilds its local view by replaying every shielded update
  (S5.1) and exposes it via the same traits. The K-recency window (S4.3) absorbs
  view lag.

S4.2's `NullifierRegistry` and S4.3's root state `impl NullifierMembership` /
`impl MerkleRootHistory`. Tests use `FakeNullifierMembership` / `FakeRootHistory`.
This is the S4.1/S4.2/S4.3 seam — the spec never touches the concrete registry.

> NOTE (ordering): because S4.1 references S4.2/S4.3 interfaces ahead of their
> construction, S4.1's discriminators for checks 2 and 3 run against the fakes;
> S4.2/S4.3 must re-run those same discriminators against their concrete
> implementations to prove the interfaces line up.

### Decision 3 — HEADLINE: reconstructing `PublicInputs` requires the *spent* commitment (a wire gap)

**Check 4 needs a `PublicInputs`**, which the circuit exposes as
`{ nullifier, commit_x, commit_y, merkle_root, output_commit_x, output_commit_y, fee }`
(`circuit.rs:380`). `ShieldedVerifier::verify(proof, &PublicInputs)` runs
`verify_proof` over the 7-column `instances` (`instances_for`, `verify.rs:144`).
Those 7 values must be reconstructed from the wire tx. Mapping (per the v1
**1-in/1-out** circuit):

| `PublicInputs` field | source from `ShieldedTransaction` | status |
|---|---|---|
| `nullifier` | `bytes32_to_fp(tx.nullifiers[0])` | ✅ |
| `commit_x, commit_y` | the **spent** note's commitment `EpAffine` → `.coordinates()` | ❌ **missing on the wire** |
| `merkle_root` | `bytes_to_root(tx.merkle_root)` (`Option`, fallible) | ✅ |
| `output_commit_x, output_commit_y` | `tx.commitments[0]` as the output `EpAffine` → `.coordinates()` | ✅ |
| `fee` | `tx.fee` | ✅ |

**The gap**: `tx.commitments` carries only the **new (output) notes**
(`transactions.rs:374`, "New notes — one per output"). The circuit's
`commit_x/commit_y` are the **spent** note's commitment (`circuit.rs:368-369`,
`note_commit = commit(&self.note)`). A nullifier is `poseidon(spend_key, rho)` —
one-way — so the spent commitment **cannot** be reconstructed from the nullifier.
The verifier must be handed it. Therefore the v1 wire type is **missing the
spent note commitment(s)**.

**Resolution (recommended, additive, rule-4-safe):** add one field to
`ShieldedTransaction`:

```rust
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ShieldedTransaction {
    // …existing…
    pub spent_commitments: Vec<[u8; 32]>, // spent notes — one per nullifier (NEW, S4.1)
    // …existing…
}
```

- Additive: old decoders skip the unknown map key (serde default); for every
  pre-upgrade (non-shielded) block the field is absent, so all existing canonical
  bytes are unchanged (S3.1's byte-identity property preserved).
- The v1 circuit is 1-in/1-out, so a valid tx has `spent_commitments.len() == 1`
  and `commitments.len() == 1`; the structural check (check 1) enforces this.
- This same field is what S5.3's Committer uses to re-derive the tree leaf
  (`poseidon_hash([x, y])`) — it is the committer's view of the spent note.

**`public_inputs_from_shielded_tx`** (new, in `shielded/`, S4.1.2):

```rust
pub fn public_inputs_from_shielded_tx(tx: &ShieldedTransaction)
    -> Result<PublicInputs, PneumaticError> {
    // nullifier
    let nullifier = bytes32_to_fp_shielded(&tx.nullifiers.get(0).ok_or(InvalidCommitment)?)?;
    // spent commitment -> commit_x/commit_y  (fails closed on undecodable point)
    let (commit_x, commit_y) = point_coords(&tx.spent_commitments.get(0).ok_or(InvalidCommitment)?)?;
    // referenced root (fallible)
    let merkle_root = bytes_to_root(&tx.merkle_root).ok_or(InvalidCommitment)?;
    // output commitment -> output_commit_x/output_commit_y
    let (oc_x, oc_y) = point_coords(&tx.commitments.get(0).ok_or(InvalidCommitment)?)?;
    Ok(PublicInputs { nullifier, commit_x, commit_y, merkle_root,
                      output_commit_x: oc_x, output_commit_y: oc_y,
                      fee: Fp::from(tx.fee) })
}
```

`point_coords(&[u8;32])` decodes a compressed affine point and fails closed on a
non-member/point-at-infinity (mirrors `tree.rs`'s `from_repr`-into-option fail
closed idiom; note.rs uses `coordinates()`). This helper is the single place that
bridges wire ↔ circuit and is fully unit-testable with the S1 primitives (no live
prove).

**Security rationale — why this field does *not* open commitment spoofing.**
`spent_commitments` is attacker-controlled, but it is only a *carrier* for bytes;
the commitment it carries is authenticated by what runs afterward, never trusted
on faith:

1. **Bound to the proof, not the field.** The field only feeds `PublicInputs`
   (specifically `commit_x/commit_y`), and `verify_shielded_proof` is then run
   against those exact values. A submitted commitment that does not match the note
   opening the prover actually knows fails check 4 (`InvalidShieldedProof`). The
   field is always checked against the proof.
2. **Merkle membership pins it to a genuine in-tree note.** The `merkle_path`
   gate is load-bearing (it enforces `computed_path_hash == public_merkle_root`,
   and the leaf is `poseidon_hash([commit_x, commit_y])` derived from the
   submitted commitment). The tree's leaves (S5.3) are `poseidon_hash` of
   *genuine* `commit(note)` points; by Poseidon's collision resistance the
   submitted coordinates must therefore equal a real note's commitment — you
   cannot claim an arbitrary point is in the tree.
3. **Spend authority is still required.** Making the proof satisfiable needs the
   note's `rho` *and* its spend key — the `nullifier_derivation` gate (also
   load-bearing) binds the public nullifier to `poseidon(spend_key, rho)`. A note
   owned by someone else is unspendable regardless of the field.
4. **The nullifier set (S4.2) blocks replay** (check 2 → `StaleNullifier`).

So a fabricator who knows no note fails check 4; someone who wants to spend a
note they do not own fails on spend authority. The field *adds* a check (it
enables the proof check that did not exist before the gap was fixed) rather than
removing one. Caveat (Decision 6): this relies on the load-bearing `merkle_path`
and `nullifier_derivation` gates, which are active in v1; the deferred
`pedersen_commitment`/`spend_auth_binding` gates would add the further guarantee
that the commitment is a *well-formed* Pedersen commitment of a valid note — an
enhancement, not one needed to close the basic spoofing vectors.

**Impact to flag to the roadmap owner:** this is a net-new wire field on a type
S3.1 marked DONE. It is a 1-line additive change to `transactions.rs`, but it
touches the wire contract (hence the wire-compat note below) and is a
**prerequisite for S4.1.4** (proof check) and **S5.3** (tree append). Adopting
it is required for check 4 to be implementable at all; skipping it would mean the
proof check cannot reconstruct `PublicInputs`.

### Decision 4 — Error variants (exact trigger per variant)

Additive enum variants (`errors.rs`), serde-tagged → new→new backward-safe; new→old
decoders are covered by the fail-closed unknown-action gate.

| Variant | Fired by | Notes |
|---|---|---|
| `InvalidCommitment` | **Check 1** (structural) | empty/nullifier-count>cap/malformed encoding/nullifiers-not-distinct |
| `StaleNullifier` | **Check 2** (double-spend) | nullifier already in the set |
| `StaleMerkleRoot` | **Check 3** | referenced root older than K-recency |
| `InvalidShieldedProof` | **Check 4** | `verify_shielded_proof` returned `Err`/`false` |
| `UnknownNullifier` | *reserved* | fired by the **Committer's authoritative** check (S5.3) when a nullifier has no tree leaf; the proof already binds nullifier↔note-membership, so the sentinel's advisory path does **not** fire it — kept for the commit-time path |
| `ValueBalanceMismatch` | *reserved* | value is hidden inside the Pedersen commitment, so the network **cannot** independently sum values; the in-circuit value-balance gate (circuit.rs `value_balance`) is what enforces it. S4.1 does **not** fire this; it is retained for symmetry / a future exposed-value check |

The four advisory triggers (`InvalidCommitment`, `StaleNullifier`,
`StaleMerkleRoot`, `InvalidShieldedProof`) are exactly the four checks. The two
reserved variants are added now (harmless, additive) so S5.3 already has them.

### Decision 5 — Verifier construction cost & width

`ShieldedVerifier::new` runs `keygen_vk` (~60–100s) the first time, then caches
by circuit-config fingerprint (`verify.rs:64`). **The spec must never rebuild it
per transaction.**
- Hold a `once_cell::sync::Lazy<ShieldedVerifier>` built from a **template**
  circuit — `ActionCircuit::without_witnesses()` — which supplies only the
  *structure* `keygen_vk` needs (no witness data). Built once, lazily, on first
  use. Per-tx `verify` only calls `instances_for(&PublicInputs)` + `verify_proof`.
- **Width `k` must match the prover.** S3.3 proves at `k = 10` (`circuit_test.rs`);
  the verifier must build at `k = 10` or `verify_proof` rejects everything. Pin
  `const VK_K: u32 = 10`; a `#[test]` asserts the verifier's `params.k() == 10`.
- **Cost gating**: building the `Lazy` is the one expensive op and must not run in
  the default suite per-test. A `#[ignore]`d discriminator (mirroring
  `verify.rs`'s `verify_vk_cache_reused_across_verifiers`) proves `keygen_vk` runs
  exactly once across N verifier constructions.

### Decision 6 — SOUNDNESS CAVEAT (must be stated honestly)

Several in-circuit gates are **no-ops in v1** (`circuit.rs`): `poseidon_sponge`,
`pedersen_commitment`, `output_well_formed`, and `spend_auth_binding` all return
`vec![selector * Fp::zero()]` — *no constraint*. Only `merkle_path`,
`nullifier_derivation`, and `value_balance` (plaintext `note_value -
output_value - fee`) are load-bearing today.

**Consequence for S4.1:** `InvalidShieldedProof` (check 4) as currently
constructible guards against **malformed transcripts and wrong public inputs**,
but **not** against the soundness violations the *complete* circuit would catch
(Pedersen-binding forgeries, spend-auth forgeries, in-circuit Poseidon
commitment rewriting). The proof check is **as strong as S2.1's circuit**. The
plan flags this as the top risk: S4.1's "flip-a-constraint" discriminator (ground
rule 2) can only exercise the *currently load-bearing* gates until S2.1 completes
the no-op gates. Until then, shielded txs are verified as *structurally and
transcript-valid*, not fully SNARK-sound.

---

## 4. Build items (atomic, each with a discriminator)

Baseline: **727 passed** (live-measured today: core 489, committer 71, sentinel
57, finalizer 55, node-server 30, executor 10, pipeline_integration 9,
transport_integration 6). Progression tracked below; existing validation tests
pass **untouched** (purely additive).

### S4.1.1 — Error variants
- **Files**: `src/errors.rs`.
- **Action**: Add `InvalidShieldedProof`, `StaleNullifier`, `StaleMerkleRoot`,
  `InvalidCommitment` to `ValidationFailureReason`, plus the two reserved
  `UnknownNullifier`, `ValueBalanceMismatch` (empty discriminators; S5.3 reserves
  `UnknownNullifier`). Additive; serde-tagged; no behavior change.
- **Verify**: `cargo check --workspace` clean.
- **Done (discriminator + progression)**: `new_shielded_reasons_are_distinct_and_greppable`
  — build a `PneumaticError::Validation` carrying each new variant and assert its
  `format!("{:?}", …)` contains the exact variant name (proves they're real,
  greppable, distinct). Revert the additions → the test **fails to compile**
  (discriminator). **+1 default: 727 → 728.**

### S4.1.2 — `PublicInputs` reconstruction + additive `spent_commitments` field
- **Files**: `src/transactions.rs` (add `spent_commitments`), `src/shielded/verify.rs`
  (new `public_inputs_from_shielded_tx`; or `shielded/mod.rs` re-export).
- **Action**: (1) add the additive `spent_commitments: Vec<[u8;32]>` field to
  `ShieldedTransaction`. (2) `public_inputs_from_shielded_tx(tx)` decodes all 7
  columns and fails closed on any bad encoding / missing element / undecodable
  point / bad root (`bytes_to_root` `None`). Reuses `bytes_to_root` + the
  `EpAffine`/`.coordinates()` decode.
- **Verify**: a well-formed tx maps to the correct `PublicInputs`; every failure
  mode returns `Err(InvalidCommitment)`.
- **Done (discriminator + progression)**:
  - `public_inputs_from_tx_maps_allseven_columns` (2-field sanity: nullifier,
    root, output coords, fee).
  - `public_inputs_rejects_bad_root_encoding` (discriminator: a `merkle_root`
    that `bytes_to_root` can't represent → `Err`, never a panic).
  - `public_inputs_rejects_empty_nullifier_or_spent_commitment` (discriminator:
    empty vec → `InvalidCommitment`; proves the check-1 bounds live in the
    bridge, not downstream). **+3 default: 728 → 731.**

### S4.1.3 — Spec skeleton + check 1 (structural) + risk + `register_shielded`
- **Files**: `src/validation.rs`.
- **Action**: (1) define `ShieldedValidationSpec { name }` and the dependency
  traits `NullifierMembership`, `MerkleRootHistory`, `RootSnapshot`,
  `ShieldedValidationDeps`. (2) `impl TransactionValidationSpec` for it:
  `validate(&Transaction,…)` fails closed (`UnsupportedAction`); `validate_shielded`
  runs check 1; `name()` returns `"Shielded"`. (3) **structural check 1**: counts
  non-empty and ≤ `CIRCUIT_MAX_INPUTS` (v1 = 1; parameterized const so it moves in
  one place when the circuit generalizes — *S3.3 Open item 1*); `merkle_root`
  decodes; commitments decode; `nullifiers` pairwise distinct within the tx → else
  `InvalidCommitment`. (4) fixed neutral risk + `max_risk` gate. (5)
  `ValidationSpecRegistry::register_shielded()` registers `"Shielded"`; **not** in
  `register_defaults`.
- **Verify**: each structural defect → `InvalidCommitment`; a clean tx passes
  structurally; registry holds `"Shielded"` only after `register_shielded()`.
- **Done (discriminator + progression)**:
  - `structural_ok_for_valid_1in1out_shape` (empty structure passes; risk is
    neutral `affected_parties:2, amount:0`, score computed deterministically).
  - `structural_rejects_empty_nullifiers` (discriminator: `nullifiers: []` →
    `InvalidCommitment`).
  - `structural_rejects_nullifier_count_over_circuit_cap` (discriminator: with a
    configurable cap, 2 nullifiers → `InvalidCommitment`; also the
    pairwise-distinct discriminator: duplicate nullifier → `InvalidCommitment`).
  - `register_shielded_adds_spec_without_touching_defaults` (discriminator: after
    `register_defaults()` alone, `get("Shielded")` is `None` → a shielded tx is
    rejected `UnsupportedAction`; after `register_shielded()`, it is present).
  **+4 default: 731 → 735.**

### S4.1.4 — Check 4 (proof) + VK `Lazy` + width match
- **Files**: `src/validation.rs`, `src/shielded/verify.rs`.
- **Action**: (1) module-level `Lazy<ShieldedVerifier>` built from
  `ActionCircuit::without_witnesses()` at `VK_K = 10`; `validate_shielded` calls
  `verifier.verify(&tx.proof, &public_inputs_from_shielded_tx(tx)?)`. (2) Never
  panics on untrusted proof bytes — a malformed proof is `Err(Shielded(..))`
  surfaced as `InvalidShieldedProof` (mirrors `verify_sender_signature`). (3) A
  `#[test]` asserts `verifier.params.k() == 10` (must match the prover).
- **Verify**: garbage/tampered proof → `InvalidShieldedProof`; a different root in
  the reconstructed inputs → `InvalidShieldedProof`.
- **Done (discriminator + progression)**:
  - `proof_check_rejects_garbage_proof` (discriminator: empty/all-zero `proof` →
    `InvalidShieldedProof`; **default suite**, no live prove needed — mirrors
    `verify_fails_closed_on_garbage_proof`).
  - `proof_check_rejects_tampered_public_input` (discriminator: reconstruct with
    a flipped `merkle_root` → `InvalidShieldedProof`; proves check 4 binds the
    root, not just the proof bytes).
  - `verifier_is_built_at_k10` (discriminator: `params.k() != 10` would reject a
    real proof; asserting `== 10` pins the prover↔verifier width).
  - `#[ignore]`d `proof_vk_cache_reused_once` (proves `keygen_vk` runs once across
    N constructions — mirrors `verify.rs`'s ignored test; **benchmark-only**).
  **+3 default (+1 ignored): 735 → 738.**

### S4.1.5 — Check 2 (nullifier) + check 3 (root freshness), wired to interfaces
- **Files**: `src/validation.rs`.
- **Action**: finish `validate_shielded`: (2) each nullifier
  `!deps.spent.contains_nullifier(n)`, else `StaleNullifier`; (3) `merkle_root`
  within `recency_window` of the newest `deps.roots.root_history()`, else
  `StaleMerkleRoot`. Uses the fakes (`FakeNullifierMembership`, `FakeRootHistory`).
- **Verify**: each check rejects with its exact reason; the K-window is real (not
  just equality).
- **Done (discriminator + progression)**:
  - `nullifier_check_rejects_already_spent` (discriminator: pre-insert the
    nullifier into the fake set → `StaleNullifier` — **proves check 2 runs, not
    just check 4**).
  - `root_freshness_accepts_current_and_rejects_stale` (discriminator: set
    `recency_window = 0` → a K-1-old root now rejects, proving the window logic is
    load-bearing, not implicit equality).
  - `shielded_tx_passes_nullifier_and_root_with_fakes` (happy path through checks
    2 and 3 with a fresh nullifier and a current root).
  **+3 default: 738 → 741.**

### S4.1.6 — Ordering + fail-closed registration + end-to-end live-prove
- **Files**: `src/validation.rs`.
- **Action**: wire the four checks into one short-circuiting `validate_shielded`;
  add the fail-closed "no registered spec" behavior; add the single end-to-end
  `#[ignore]`d live-prove happy-path test.
- **Verify**: checks fire in order 1→2→3→4; a fully valid tx (from a real proof)
  passes all four.
- **Done (discriminator + progression)**:
  - `checks_run_in_structural_nullifier_order` (discriminator: a tx that is
    **both** structurally invalid *and* carries a stale nullifier →
    `InvalidCommitment`, not `StaleNullifier`; a structurally valid but stale-
    nullifier + bad-proof tx → `StaleNullifier`, proving check 2 precedes check 4).
  - `register_shielded_missing_rejects_fail_closed` (discriminator: a `"Shielded"`
    tx with only `register_defaults()` → `UnsupportedAction`, mirroring Phase 3.2).
  - `#[ignore]`d `validate_shielded_end_to_end_live_prove` (real
    `build_shielded_tx`/proof from the fixture → `validate_shielded` returns
    `Ok` through all four checks; flip the proof → `InvalidShieldedProof`. The
    ONE live prove for S4.1; runs on demand
    `cargo test -p pneumatic_core -- --ignored shielded_end_to_end_live_prove`).
  **+2 default (+1 ignored): 741 → 743.**

**S4.1 verification (this phase).** `cargo check --workspace` clean (no lockfile
growth — halo2 was already promoted); `cargo test --workspace` = **743 passed,
+2 ignored** (both live-prove), up from the 727 baseline. Every increment names
its discriminator. The two `#[ignore]`d live-prove tests run on demand:

```text
cargo test -p pneumatic_core -- --ignored proof_vk_cache_reused_once
cargo test -p pneumatic_core -- --ignored shielded_end_to_end_live_prove
```

---

## 5. Wire-compat note (ground rule 4) — applies to S4.1.2

Additive only. The **only** wire primitive change this phase makes is one new
field on `ShieldedTransaction` — `spent_commitments: Vec<[u8;32]>`:
- serde `default` + `skip_serializing_if = Vec::is_empty`; old decoders skip the
  unknown key; for every non-shielded block the field is absent, so all existing
  canonical byte sequences (`canonical_signed_trans_bytes`, `Transaction::hash`,
  block hashes) stay **byte-identical** (S3.1's property preserved).
- `ShieldedTransaction` has no `HashMap`, so its serde form is already canonical;
  the new `Vec` field is appended deterministically.
- No framing change (rides the existing 4-byte length prefix). No new action
  string (S3.2 already defined `"ShieldedTransfer"`). Old nodes still fail
  closed on `"ShieldedTransfer"` (S3.2); a `spent_commitments`-less shielded tx
  would now fail check 1 (`InvalidCommitment`) — the safe direction until every
  prover emits the field.

---

## 6. Risks & open items

1. **Spent-commitment wire field (Decision 3).** A net-new wire field on a type
   S3.1 marked DONE. Small and additive, but it's a real contract change. **Flag
   to roadmap owner**: adopt `spent_commitments` now (S4.1.2) so check 4 is
   implementable; it's also S5.3's tree-leaf source. Not adopting it forces an
   S2.1 circuit change (removing `commit_x/commit_y` as public inputs), which is
   the more expensive path.
2. **Soundness gap (Decision 6).** v1 circuit gates (`pedersen_commitment`,
   `spend_auth_binding`, `poseidon_sponge`, `output_well_formed`) are no-ops, so
   `InvalidShieldedProof` is *structural/transcript* verification, not full
   SNARK soundness. This closes as S2.1 completes those gates. **Budget extra
   review** on the circuit (S2.1's plan allocates review time for exactly this).
3. **Verifier cost / width lock.** `Lazy<ShieldedVerifier>` at `k=10` is built
   once; a `cargo update` bumping any Halo2/Pasta crate is an API-migration event
   (mirror the `=`-pins in `Cargo.toml`). A `k` mismatch between prover and
   verifier rejects *every* proof — pinned + tested.
4. **v1 circuit is 1-in/1-out.** `CIRCUIT_MAX_INPUTS = 1` in S4.1.1. A 2-in/2-out
   transfer needs a wider circuit (an S2.1 change, *not* S4.1) or a documented v1
   cap batched at the wallet layer (S3.3 Open item 1). The structural check's cap
   is the single enforcement point.
5. **`UnknownNullifier` / `ValueBalanceMismatch` are reserved.** Added now (additive)
   for the Committer's authoritative path (S5.3) and symmetry; the sentinel's
   advisory path does not fire them. Revisit at S5.3.
6. **Interface line-up with S4.2/S4.3.** S4.1's check-2/check-3 discriminators run
   against fakes; S4.2/S4.3 must re-run them against `NullifierRegistry` / root
   state to prove `NullifierMembership` / `MerkleRootHistory` line up.

---

## 7. Relationship to the rest of the shielded plan

- **Prerequisites (already built):** S1 (primitives), S2.1/S2.2 (`ActionCircuit`,
  `ShieldedVerifier`), S3.1 (`ShieldedTransaction`, `SignedTransaction.shielded`),
  S3.2 (`"ShieldedTransfer"`).
- **This phase (S4.1):** error variants, the spec, the four checks, the deps
  interfaces, the `PublicInputs` bridge + `spent_commitments` field.
- **Depends-on (S4.2 / S4.3):** `NullifierRegistry` (impl `NullifierMembership`),
  root history (impl `MerkleRootHistory`) — the concrete data the checks read.
- **Consumes this (S5):** S5.1 sentinel branch calls `spec.validate_shielded(...)`
  as an advisory pre-check; S5.2 finalizers re-verify (check 4) and sign; S5.3
  Committer re-checks all four (authoritative, guarded apply) and appends the new
  commitments using `spent_commitments` + `commitments`.

**S4.1 sub-total: ~20h** (dominated by S4.1.4's verifier plumbing and the S2.1
soundness review; the checks themselves are small once the bridge and deps exist).
