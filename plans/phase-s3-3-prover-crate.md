# Phase S3.3 Implementation Plan — `pneumatic_prover` crate

Expanded implementation plan for **Phase S3.3** of `pneumatic-shielded-implementation-plan.md`
(the `pneumatic_prover` client-side crate). This document turns the one-bullet
S3.3 entry into a checklist-form, item-by-item build plan that follows the same
`Files / Action / Verify / Done` format (discriminator + workspace test-count
progression) used throughout the parent plan.

Everything below is grounded in the code as it stands today (read 2025-09-16):
the workspace layout (`Cargo.toml`), the already-implemented `src/shielded/`
primitives (S1.x + S2.1/S2.2), the wire type `ShieldedTransaction`
(`src/transactions.rs`), and the live-prove recipe that S2.1's tests use.

---

## 1. What S3.3 is (and is not)

**In scope.** A new **client-side, non-networked** workspace member `prover/`
(`pneumatic_prover`) that a wallet runs to *produce* a shielded transfer:

- the spend/viewing **key model** (`SpendKey` → `owner_pk` + viewing key),
- `create_note` — mint an output note and its encrypted ciphertext(s),
- `build_shielded_tx` — assemble a `ShieldedTransaction` (the wire type core
  already defines) with correct nullifiers, commitments, referenced root,
  note ciphertexts, and a Halo2 proof,
- `scan_for_notes` — the viewing-key compliance/audit path (roadmap 2.6).

**Out of scope (deliberately).**
- **No network code.** The prover never touches Sentinel/Finalizer/Committer;
  it only *returns* a `ShieldedTransaction` for the client to send. (This is
  the roadmap 2.1 "proving is client-side; the network only verifies"
  decision.)
- **No wire changes.** `ShieldedTransaction` is defined in core (S3.1) and is
  unchanged by this phase. The crate only **consumes/reproduces** it. See §4.
- **No circuit design.** The `ActionCircuit` (S2.1) is built and proved via
  core's `shielded::circuit` API; this crate feeds it witnesses and consumes
  the proof. No new constraints are written here.
- **No node-side verification.** `ShieldedVerifier` (S2.2) lives in core and is
  called by the crate only inside its own `#[ignore]`d live-prove test to *check*
  what it produced — the network never calls the prover.

---

## 2. Grounding facts verified against the code

These are the exact APIs S3.3 reuses, so the implementation binds to real
signatures rather than assumptions.

| Surface | Signature (verified) | Used by S3.3 for |
|---|---|---|
| `ShieldedNote` | `struct { value: u64, owner_pk: [u8;32], rho: Fq, rcm: Fq }` (`src/shielded/note.rs:44`) | the in-note model; `Fq` = Pallas *scalar* |
| `commit` | `fn commit(note: &ShieldedNote) -> EpAffine` (`note.rs:99`) | input & output note commitments |
| `nullifier` | `fn nullifier(note: &ShieldedNote, spend_key: &[u8;32]) -> [u8;32]` (`note.rs:143`) | per-input nullifiers (pairwise distinct) |
| `NotePlaintext` | `struct { value: u64, memo: Vec<u8>, sender_pk_hint: Option<[u8;32]> }` (`note.rs:158`) | decrypted viewing payload |
| `encrypt_note_to_two` | `fn encrypt_note_to_two(pt: &NotePlaintext, spend_recipient: &Ed25519Provider, viewing_recipient: &Ed25519Provider) -> (Vec<u8>, Vec<u8>)` (`note.rs:214`) | per-output note ciphertexts (spend + viewing) |
| `decrypt_note` | `fn decrypt_note(ct: &[u8], recipient: &Ed25519Provider) -> Result<NotePlaintext, PneumaticError>` (`note.rs:200`) | viewing-key scan |
| `ActionCircuit` | `fn new(note, spend_key:[u8;32], merkle_proof, merkle_root: Fp, output_note, fee:u64, tree_depth:u32)` (`circuit.rs:308`); `.public_inputs()` (`:352`); `.verify_merkle_path_off_circuit()` (`:329`) | building + pre-validating the circuit instance per output |
| `PublicInputs` | `struct { nullifier, commit_x, commit_y, merkle_root, output_commit_x, output_commit_y, fee: Fp }` (`circuit.rs:380`) | the circuit's public-input vector |
| `IncrementalMerkleTree` | `new(depth)`; `.append(&commit) -> (Fp, MembershipProof)`; `.verify_proof(leaf, proof, root, depth) -> bool` (`tree.rs`) | Merkle membership for the spend |
| `bytes_to_root` / `root_to_bytes` | `bytes_to_root(&[u8;32]) -> Option<Fp>` (fallible) (`tree.rs:324`); `root_to_bytes(&Fp) -> [u8;32]` (`:318`) | referencing root wire ↔ circuit type |
| `MembershipProof` | `struct { index: u64, siblings: Vec<...> }` (`tree.rs:86`) | spend path; `.siblings`/`.index` are the accessors (`circuit.rs:339`) |
| `ShieldedTransaction` | wire type at `src/transactions.rs:362`; `.canonical_bytes()` (`:399`); `.hash()` (`:408`); `.placeholder_transaction()` (`:421`) | the value this crate returns |
| `ShieldedVerifier` | `ShieldedVerifier::new(circuit, k)` (`verify.rs:111`); `.verify(proof, &PublicInputs) -> Result<(), PneumaticError>` (`:163`) | the crate's own correctness check (test only) |
| Live-prove recipe | `Params::new(K)`; `keygen_vk`/`keygen_pk`; `create_proof(&params,&pk,&[circuit], instances, &mut OsRng, &mut Blake2bWrite…)` at `circuit_test.rs:327-371`; `K = 10` for the Action circuit | how `build_shielded_tx` actually produces `proof: Vec<u8>` |

**Workspace / dependency facts.**

- The prover must become a **new workspace member**: root `Cargo.toml`
  `members` gains `"prover"` (currently `[., sentinel, executor, finalizer,
  committer, node-server]`).
- `halo2_proofs`, `pasta_curves`, `ff`, `group`, `subtle` are **already
  production deps of `pneumatic_core`** (pinned: `halo2_proofs = "=0.3.5"`,
  `pasta_curves = "=0.5.2"`). `Cargo.lock` resolves each of them exactly once.
  Pinning the prover's direct copies to the **same `=`-versions** introduces
  **zero new packages** to `Cargo.lock` — same zero-delta philosophy the root
  `Cargo.toml` comments (S1.2–S2.1) already use. `rand = "0.8"` is likewise
  already in the tree (core uses `rand 0.8`).
- Halo2 items are **not re-exported** by core, so the prover needs
  `halo2_proofs` and `pasta_curves` as **direct** dependencies to name
  `create_proof`, `Params`, `Blake2bWrite`, `Fq`/`Fp`, etc.
- The prover crate mirrors how the node crates take `pneumatic_core = { path = ".." }`
  (edition 2021) — but adds the SNARK surface as direct deps.

---

## 3. Wire-compat note for S3.3 (ground rule 4 of the parent plan)

S3.3 produces **no wire change.** `ShieldedTransaction` is defined and owned by
core (S3.1); the prover crate only constructs one and hands it to the client.
Therefore:

- **No new action strings, no `Transaction`/`SignedTransaction` field changes.**
  `SignedTransaction.shielded` (additive, serde `default` +
  `skip_serializing_if = Option::is_none`) already exists and is populated by the
  finalizer (S5.2) from the `ShieldedTransaction` the client submits — the prover
  fills that payload's *contents*, not its schema.
- **The two size facts the crate must respect on the wire were already sized in
  S1/S2:** the Halo2 proof (~1–2 KB) and each note ciphertext (~2.3 KB PQC-hybrid
  payload, so a 2-note transfer ≈ 4.6 KB) — comfortably under the 16 MiB frame cap,
  so no resource-transfer dependency (parent plan Context row).
- **Old nodes still fail closed** on `"ShieldedTransfer"` (S3.2); the prover has no
  bearing on that path. S3.3 is purely additive to the *proving* side.

---

## 4. Key-model design decision (the one real design point in S3.3)

The parent bullet says "`SpendKey` (32 B, seed) → derives (a) spend auth keypair
(32 B pk/sk = the note's `owner_pk`; Ed25519 keys are fine — this is *value*
authorization, not node identity), (b) viewing key (32 B, can decrypt, can't
spend), (c) no note-scoped randomization beyond per-note `rho`." This item locks
the concrete form and **resolves two tensions** the compact bullet leaves open.

### 4a. Value-auth key vs. node-identity key

`owner_pk` (the note's Pedersen key, `note.rs:49`) is a **32-byte value-
authorization** public key — it is mixed into the commitment via
`owner_pk_to_scalar` and bound to the nullifier's spend secret inside the
circuit (`circuit.rs:841,866`). It is **not** a node identity key and is *not*
the `Ed25519Provider` used for encryption. Concretely:

- `SpendKey::from_seed(seed: [u8;32])` derives, deterministically:
  - `spend_secret: [u8;32]` — the private side; the `spend_key` passed to
    `nullifier` and supplied as the circuit witness.
  - `owner_pk: [u8;32]` — the public side. v1 derivation:
    `owner_pk = sha256("pneumatic-shielded/v1/owner-pk" || spend_secret)[..32]`
    (documented; the circuit's binding constraint proves `owner_pk == H(spend_secret)`
    — that binding is **S2.1's** in-circuit responsibility, flagged in §9).
- The parent bullet's "Ed25519 keys are fine here" is honored by *allowing* the
  spend-secret/owner-pk pair to be an Ed25519 `(SigningKey, VerifyingKey)`; the
  default derivation above is kept because an in-circuit Ed25519 verify is too
  expensive, and the commitment only needs 32 arbitrary bytes.

### 4b. Encryption recipient vs. `owner_pk`

`encrypt_note`/`encrypt_note_to_two` (note.rs) encrypt to an `Ed25519Provider`'s
own hybrid keys — i.e. **"encrypt to myself."** So a recipient is addressed by
supplying *their* `Ed25519Provider`. To bridge `owner_pk` (value auth) with the
encryption identity, v1 treats the recipient's `owner_pk` as **derived from the
same seed**: the wallet supplies a `ShieldedIdentity { SpendKey, Ed25519Provider }`
for each recipient, and `build_shielded_tx` sets `output.note.owner_pk` to that
identity's `owner_pk` and encrypts to its `Ed25519Provider`. The viewing key
holder gets a separate ciphertext encrypted to the provider the auditor holds.
This is a v1 wiring choice (not a security model change — the note randomness
`rho`/`rcm` and the value are still hidden from everyone but the spend holder).
**Flagged as an open item in §9** for the roadmap owner; the technical path is
the same for owner-only or compliance viewing keys (parent plan Open Q #2).

### 4c. Randomization is per-note only

`rho` and `rcm` are **randomized per output note** at creation (`create_note`);
there is no separate note-scoped randomization scheme. Two notes with identical
`value`+`owner_pk` must still differ (different `rho`/`rcm` → different
commitment → unlinkable). Enforced and tested in S3.3.2.

---

## 5. Corrected / tightened points vs. the compact S3.3 bullet

Before the items, two correctness corrections to the bullet as written. These
are load-bearing for the plan and are called out so they are not silently
carried forward.

1. **`scan_for_notes` cannot return `Vec<ShieldedNote>` — it returns
   `Vec<NotePlaintext>` (corrected).** A viewing key only ever decrypts the note
   **plaintext** (`NotePlaintext { value, memo, sender_pk_hint }`, note.rs:158).
   It never sees the note's private randomness `rho`/`rcm` — those stay with the
   creator and are what make the commitment binding. Reconstructing a
   `ShieldedNote` would require fabricating `rho`/`rcm`, which is impossible and
   would corrupt the view. So the corrected signature is
   `scan_for_notes(ciphertexts: &[u8], viewing_key_provider: &Ed25519Provider) ->
   Result<Vec<NotePlaintext>, PneumaticError>`. This is a fail-closed property:
   a corrupt/mismatched ciphertext yields `Err` (GCM tag), never a guessed note.
   The discriminator test pins that scan returns *plaintext*, not a `ShieldedNote`.

2. **The "one live prove" is gated `#[ignore]`, matching S1/S2/S5/S6.** The
   parent plan's Phase-gate note says "S3 done = prover crate builds a verifying
   tx (**one live prove**)", and roadmap Part 5 says proving is benchmark-only.
   The established workspace discipline (S1's `shielded_live_prove_smoke`,
   S2.1's `circuit_live_prove_smoke`, S2.2's `shielded_live_verify`) keeps the
   single live prove **`#[ignore]`d** and runs only setup + local satisfiability
   in the default suite. S3.3 follows: the default suite tests every *cheap*
   property (key derivation, note randomness, ciphertext roundtrip, scan,
   Merkle off-circuit, wire-type assembly from a canonical proof); the single
   `#[ignore]`d test runs the real `build_shielded_tx` prove + `ShieldedVerifier::
   verify`. The S6.1 integration test is the *other* sanctioned live prove for
   the end-to-end pipeline; S3.3's is the prover's own.

---

## 6. Build items

Baseline before S3.3: **`cargo test --workspace` = 727 passed** (live-measured;
the 663 figure is the plan-writing snapshot, preserved in the parent plan's
Context). S3.3 adds a dedicated test set in `prover/src/`; every `#[test]`
below is counted in the workspace total, so the progression is
**727 → 727 + N_default + 1_ignored**.

### S3.3.1 — Crate scaffolding + workspace member + `SpendKey` model

- **Files**: root `Cargo.toml` (`members += "prover"`); new `prover/Cargo.toml`;
  `prover/src/lib.rs`; `prover/src/key.rs`.
- **Action**:
  1. Register `prover` as a workspace member (root `Cargo.toml`).
  2. `prover/Cargo.toml`: `package = "pneumatic_prover"`, `edition = "2021"`,
     deps `pneumatic_core = { path = ".." }`, `halo2_proofs = "=0.3.5"` (direct —
     to name `create_proof`/`Params`/`Blake2bWrite`), `pasta_curves = "=0.5.2"`
     (direct — `Fq` on `ShieldedNote`, `Fp` on `PublicInputs`/`ActionCircuit`),
     `rand = "0.8"`, `once_cell = "1.21"`, `serde`/`serde_json` (optional),
     `sha2 = "0.10"` (key derivation). All versions pinned to those already
     resolved by `pneumatic_core` → **zero new lockfile packages**.
  3. `key.rs`: `SpendKey { seed: [u8;32] }` with `from_seed`, `spend_secret() ->
     [u8;32]`, `owner_pk() -> [u8;32]`, `viewing_key() -> [u8;32]`, and a
     `ShieldedIdentity { spend: SpendKey, identity: Ed25519Provider }` that
     bundles the value-auth key with the encryption provider (§4b). Derivation
     pinned in code comments (sha256 of seed → spend_secret; `owner_pk = H(
     "pneumatic-shielded/v1/owner-pk" || spend_secret)`; `viewing_key = H(
     "pneumatic-shielded/v1/viewing-key" || seed)`).
- **Verify**: `cargo check --workspace` clean (prover compiles against core).
- **Done (discriminator + progression)**:
  - `key_derivation_is_deterministic` — same seed yields identical
    `spend_secret`/`owner_pk`/`viewing_key` across two `SpendKey`s. **Discriminator:**
    change the seed → all three derived values change (proves the derivation is
    seed-bound, not constant).
  - `owner_pk_is_derivable_from_spend_secret` — the documented
    `owner_pk == H(spend_secret)` relation recomputes in the test (the in-circuit
    version is S2.1's; this pins the *off-circuit* contract the prover satisfies).
  - **Progression**: +3 default tests. 727 → 730.

### S3.3.2 — Note creation + ciphertexts (`create_note`)

- **Files**: `prover/src/note_builder.rs`.
- **Action**:
  1. `create_note(recipient: &ShieldedIdentity, value: u64) -> (ShieldedNote, Vec<u8>)`:
     - randomize `rho: Fq`, `rcm: Fq` with `rand` (never reused within the tx —
       see below),
     - build `ShieldedNote { value, owner_pk: recipient.owner_pk, rho, rcm }`,
     - `commit(&note)` for the output commitment,
     - `encrypt_note_to_two` the `NotePlaintext { value, memo, sender_pk_hint }`
       to `recipient.identity` (spend) and the supplied viewing provider (viewing);
       return the **spend-side** ciphertext (the wire field
       `ShieldedTransaction.note_ciphertexts` carries one ciphertext per output).
  2. `build_output_note`/`build_input_note` helpers used by `build_shielded_tx`;
     input notes are supplied wholesale (the spender already holds them), only
     re-randomization applies when re-issuing.
- **Verify**: note commitment matches `commit`; ciphertext roundtrips.
- **Done (discriminator + progression)**:
  - `create_note_commitment_matches_core` — the output note's commitment equals
    `pneumatic_core::shielded::commit(&note)` (proves the prover builds the note
    core's tree will accept at append time, S5.3).
  - `create_note_randomizes_rho_and_rcm` — two `create_note` calls with the same
    `value`+`owner_pk` produce **different** `rho`/`rcm` and therefore different
    commitments (unlinkability; a constant `rho`/`rcm` would let two identical
    transfers be linked).
  - `note_ciphertext_roundtrip_spender_and_viewing` — `encrypt_note_to_two` →
    spend provider decrypts to the `NotePlaintext`; a viewing provider decrypts
    to the same plaintext.
  - `note_ciphertext_wrong_key_fails_closed` — decrypt a ciphertext with a
    foreign provider → `Err` (GCM tag), never garbage plaintext (reuses S1.5's
    fail-closed property; **discriminator** for the encryption reuse).
  - **Progression**: +4 default tests. 730 → 734.

### S3.3.3 — `build_shielded_tx` (wire assembly + prove)

- **Files**: `prover/src/build.rs` (`ProvingKey` holder + `build_shielded_tx`),
  `prover/src/lib.rs` (re-exports).
- **Action**:
  1. `ProvingKey` — a `once_cell` `Lazy` that builds the `ActionCircuit` proving
     key **once** via `Params::new(10)` + `keygen_pk` (the S2.1 recipe). The
     key is a build-time singleton per process; keygen cost (~tens of seconds)
     is amortized across all txs and **never** runs on the default test path
     (see verify).
  2. `build_shielded_tx(inputs, spend_keys, merkle_proofs, root, outputs,
     recipients) -> Result<ShieldedTransaction, PneumaticError>`, in order, all
     fail-closed:
     - pre-validate every spend's Merkle path with
       `ActionCircuit::verify_merkle_path_off_circuit()`; a bad path → `Err`
       (never prove against a non-membership path),
     - convert `root: [u8;32] → Fp` with `bytes_to_root` (returns `Option<Fp>`;
       `None` → `Err` — fallible, fail-closed),
     - assemble one `ActionCircuit` per input note paired with its single output
       (v1: one-in/one-out circuit instance; a two-in/two-out tx is built as the
       documented v1 shape where `build_shielded_tx` takes the input/output
       lists and the circuit is instantiated for the aggregate — see §9 for the
       multi-output caveat),
     - derive each nullifier via `nullifier(&note, spend_secret)`, assert
       **pairwise distinct** within the tx (structural double-spend guard),
     - derive each output commitment via `commit(&output_note)`,
     - encrypt each output to its recipient (S3.3.2),
     - run `create_proof` over the public inputs → `proof: Vec<u8>`,
     - populate `ShieldedTransaction { id, action:"ShieldedTransfer", token_id,
       nullifiers, commitments, merkle_root: root_to_bytes(root), proof,
       note_ciphertexts, fee }` and return it.
  3. A `assemble_tx(...)` helper that wires the fields from a **canonical proof
     buffer** (no live prove) — used by the default-suite wiring test so the
     assembly, hash, and canonical-bytes properties are exercised without the
     expensive prove.
- **Verify**: the returned `ShieldedTransaction` is well-formed and hash-stable;
  the single live-prove test proves + verifies it.
- **Done (discriminator + progression)**:
  - `build_shielded_tx_wire_type_is_well_formed` (default suite, via
    `assemble_tx`) — 2 inputs/2 outputs → `nullifiers.len()==2`,
    `commitments.len()==2`, `note_ciphertexts.len()==2`, `action=="ShieldedTransfer"`,
    `merkle_root` is the LE encoding of the referenced root; `hash()` is stable
    across a serde round-trip (reuses core's `ShieldedTransaction::hash`
    discriminator from `transactions.rs:1062`).
  - `build_shielded_tx_value_balance_off_circuit` (default suite) — asserted
    off-circuit that `sum(input values) == sum(output values) + fee` (belt-and-
    suspenders; the proof is the authoritative gate, S2.1).
  - `build_shielded_tx_nullifier_is_spend_secret_bound` (default suite) —
    `nullifier(note, wrong_spend_secret) != nullifier(note, spend_secret)`; a tx
    built with a spend secret that does not bind an input note yields a
    nullifier that would never match the circuit's binding — the **structural**
    discriminator for "wrong spend key."
  - `merkle_path_off_circuit_accepts_and_rejects` (default suite) —
    `verify_merkle_path_off_circuit` is `true` for a real membership proof and
    `false` for a tampered sibling (reuses S1.6's fail-closed property).
  - `build_shielded_tx_live_prove_verifies` (**`#[ignore]`d**, the one live
    prove) — run the real `build_shielded_tx` → real Halo2 proof →
    `ShieldedVerifier::new(circuit, 10).verify(proof, &public_inputs)` returns
    `Ok(())`; then flip one nullifier byte and re-derive public inputs → verify
    returns `Err` (fails closed). **Discriminator** for "builds a verifying tx."
  - **Progression**: +5 default + 1 ignored. 734 → 739 default (+1 ignored).

### S3.3.4 — Viewing-key scan (`scan_for_notes`)

- **Files**: `prover/src/scan.rs`.
- **Action**: `scan_for_notes(ciphertexts: &[Vec<u8>],
  viewing_provider: &Ed25519Provider) -> Result<Vec<NotePlaintext>, PneumaticError>`
  — decrypt each supplied ciphertext with the viewing key (S3.3 correction #1);
  keep only the ones that decrypt (a non-matching ciphertext `Err`s, fail-closed;
  the client filters/captures per-ciphertext). Returns `NotePlaintext`s — never
  a reconstructed `ShieldedNote` (§5 correction #1).
- **Verify**: a ciphertext created for this viewing key decrypts to the right
  value; a foreign viewing key yields `Err`.
- **Done (discriminator + progression)**:
  - `scan_for_notes_returns_plaintext_not_note` — asserts the result is
    `NotePlaintext` (carries `value`/`memo`/`sender_pk_hint`) and that **no**
    `rho`/`rcm` are recoverable from the viewing side (structural proof that the
    audit escape hatch reveals value but not the note randomness). **Primary
    discriminator** for the viewing path.
  - `scan_for_notes_wrong_viewing_key_rejected` — wrong viewer provider → `Err`.
  - **Progression**: +2 default tests. 739 → 741.

### S3.3.5 — Reuse verification API in the crate's own test + integration seam

- **Files**: `prover/src/build.rs` test module (uses `ShieldedVerifier` from
  core; already read at `verify.rs`), `prover/src/lib.rs` (re-export the public
  surface: `SpendKey`, `ShieldedIdentity`, `create_note`, `build_shielded_tx`,
  `assemble_tx`, `scan_for_notes`).
- **Action**: confirm the prover's proof shape is exactly what `ShieldedVerifier`
  consumes — same `K=10`, same instance-column order
  (`ShieldedVerifier::instances_for`: nullifier, commit_x, commit_y, merkle_root,
  output_commit_x, output_commit_y, fee — verify.rs:144). The prover builds its
  `PublicInputs` from `ActionCircuit::public_inputs()`; a unit test asserts the
  two orderings agree (`verify_instances_match_circuit_columns` proves this in
  core; restate it as a prover-side invariant test so a future circuit change
  breaks the prover, not the network).
- **Verify**: `ShieldedVerifier::instances_for(&prover_public_inputs)` equals
  `ActionCircuit::public_inputs()` column order.
- **Done (discriminator + progression)**:
  - `prover_public_inputs_match_verifier_columns` (default suite) — the prover's
    public-input ordering equals `ShieldedVerifier::instances_for` ordering
    (discriminator for the prove/verify layout contract).
  - **Progression**: +1 default test. **741 → 742 default (+1 ignored).**

---

## 7. Final verification (this phase)

- `cargo check --workspace` clean (prover compiles; no lockfile growth).
- `cargo test --workspace` = **742 passed, +1 ignored** (live prove), up from
  the **727** baseline; every increment above names its discriminator.
- The **single `#[ignore]`d live prove** (`build_shielded_tx_live_prove_verifies`)
  runs on demand: `cargo test -p pneumatic_prover -- --ignored`.
- Gate for "S3 done" in the parent plan's verification list is satisfied: the
  prover crate builds a verifying tx (one live prove) and the wire roundtrips
  hold.

---

## 8. Risks

- **Live-prove cost.** `build_shielded_tx` keys off a `Lazy` proving key;
  `keygen_pk` is expensive and must stay off the default test path (enforced by
  §3.3.3 item 3 / §5 correction #2). If proving at `K=10` is unacceptably slow
  for wallet UX, that's the S6.4 benchmark finding to escalate — **not** to
  silently compress here (parent plan S2.1 budget guard).
- **Halo2 dependency hygiene.** Pinning to the exact `=`-versions keeps halo2 out
  of new lockfile entries, but the prover is the first crate to name
  `create_proof`/`Params` directly in a non-test module; a `cargo update` that
  bumps any of the 5 already-resolved Halo2/Pasta crates would be an API-migration
  event (mirror the `=`-pin philosophy used for `rns-*` in the root `Cargo.toml`).
- **Multi-output circuit** — see next section.

---

## 9. Open items / decisions to flag to the roadmap owner

1. **Per-output circuit vs. aggregate.** The S2.1 `ActionCircuit` (circuit.rs) is
   **one input note + one output note** per instance (the Orchard action-circuit
   shape generalized to 1-in/1-out for v1). A real 2-in/2-out transfer therefore
   needs **either** (a) a wider circuit that unrolls N inputs/outputs (an S2.1
   circuit-design change, **not** S3.3), **or** (b) building the v1 tx as a single
   1-in/1-out instance and treating multi-input transfers as a documented v1
   limitation (batch at the wallet layer). **Recommend flag to roadmap owner:**
   lock the v1 cap on nullifiers/commitments here (≤ circuit capacity) and track
   the general multi-output circuit as an S2.1 follow-up, exactly as the parent
   plan's S4.1 field bounds and S2.1 circuit capacity note imply.
2. **`owner_pk == H(spend_secret)` binding is off-circuit in this crate.** The
   prover asserts the relation for its own witness construction, but the
   **soundness** of that binding (a spender who forges `owner_pk` independent of
   `spend_secret`) is enforced by the Action circuit, **S2.1**. If S2.1's circuit
   does not bind `owner_pk` to `spend_secret`, this is a soundness hole to catch
   in the S2.1 circuit audit (parent plan Open Q #1), not something S3.3 can close.
3. **Viewing-key policy / recipient wiring (§4b).** v1 bundles each recipient's
   `owner_pk` with its `Ed25519Provider` into `ShieldedIdentity`; who may hold
   viewing keys (owner-only vs. compliance) is **policy only** and tracked in the
   parent plan's Open Q #2. The technical path supports both.
4. **Proof/verification benchmark (S6.4 gating).** The `#[ignore]`d live prove is
   the probe for S6.4's "seconds-scale proving acceptable?" product question — do
   not ship S3.3 as the measured proving path; surface the numbers at S6.4.
