# Implementation Plan — Pneumatic Shielded Value Transfer (Tier 1)

## Context

`pneumatic-shielded-roadmap.md` (repo root) specifies Zcash-style privacy for
Pneumatic: a user sends a token transfer where the network confirms validity
(correct spend authority, no double-spend, balance conservation) without any
node — Sentinel, Executor, Finalizer, or Committer — ever seeing sender,
receiver, or amount. The deliverable is **Tier 1 only: shielded value
transfer of self-signed tokens.** Tier 2 (private contract execution) is
explicitly out of scope per the roadmap — it is a separate research
initiative, not a line item.

Six design decisions in roadmap Part 2 are locked and drive this plan:
- **2.1** Proving happens client-side (new `pneumatic_prover` crate); the
  network only *verifies* proofs. The Executor stage is skipped entirely for
  shielded transactions.
- **2.2** Halo2 (no trusted setup, Rust-native) over Groth16/BN254.
- **2.3** Note-based (UTXO-like) state model, not shielded account balances.
- **2.4** ONE shared global shielded pool (single Merkle commitment tree +
  one nullifier set), not per-token shielding — per-token anonymity sets
  would be negligible.
- **2.5** The nullifier set is a consensus-critical, append-only, durable,
  globally-agreed data structure — first-class in `committer`, not a
  bolt-on `HashSet`.
- **2.6** Viewing keys (compliance/audit escape hatch) included from the
  start. **Decided with the product owner: full in v1** — dual-recipient
  note encryption (S1.5) and the prover's `scan_for_notes` scanning API
  (S3.3). The remaining product question is *policy only* (who may hold
  viewing keys), tracked in Open questions below.

### Ground rules (from AUDIT_CHECKLIST.md — govern every item below)

1. `cargo check` + **full workspace test suite** must pass after **every
   item**, not just at phase boundaries.
2. Every item ships **≥1 regression test that fails without the change**
   (a "discriminator" test, proven by temporarily reverting the fix).
3. Existing tests encoding old behavior must be updated as part of the item.
4. **No wire message shape changes without a wire-compat note** appended.
5. **Fail closed, not open** — missing/unknown validator, spec, identity,
   nullifier state, or proof input is an error; never a silent accept.

Checklist item format used throughout: **Files / Action / Verify / Done
(with discriminator + workspace test-count progression)**.

### Baseline (measured when this plan was written)

- Fresh `cargo test --workspace` run: **exit 0, 0 failures**. Exact
  suite-level counts: core lib **429**, committer lib **71** +
  `pipeline_integration` **9**, sentinel **56**, finalizer **55**, executor
  **10**, node-server **27**, `transport_integration` **6** (+1 ignored
  `rns_live`). **Workspace total: 663 passed.** The AUDIT_CHECKLIST's last
  recorded count (659, Phase 8.4) is slightly stale; **663 is the baseline
  for this plan's test-count progression**. The roadmap's "267 tests" figure
  is far stale and must not be used.
- **No SNARK dependencies exist**: `Cargo.lock` (227 packages) contains no
  halo2/halo2curves/pasta/ark/poseidon entries. S1.1 must confirm
  coexistence with `ed25519-dalek` 2.0, `ring` 0.17, `aes-gcm` 0.11, and the
  `pqcrypto-*` crates in a single workspace build.

### Roadmap-vs-reality reconciliations (the roadmap predates these)

| Roadmap assumption | Current reality | Plan consequence |
|---|---|---|
| "X25519 + AES-256-GCM hybrid encryption" for notes | Hybrid is now **PQC**: sigs `(Ed25519 · ML-DSA-44)` = 3796 B; KEM `(X25519 · ML-KEM-768)` + AES-256-GCM = 2332 B empty | Note encryption reuses `Ed25519Provider::encrypt_to`/`decrypt_from` as-is (stronger than planned); **note ciphertext is ~2.3 KB, not ~44 B** — wire and pool sizing must account for this |
| Four node crates only | **`node-server`** composite crate hosts all roles: `RoleDispatcher`, per-role `allowed_actions`, `route_data_plane`, `build_runtime` | S5 routing changes must update role `allowed_actions` sets and `RoleDispatcher`; any new action string must be added to the committer's `authenticate_message` role→action map (fail-closed gate from Phase 1.3) |
| Self-signed tokens skip Executor+Finalizer, go straight to Committer | Routing is spec-selected on `token.is_self_verified`; shielded routing is a *third* branch | Shielded branch keyed off `action == "ShieldedTransfer"` (see wire note), not off token type — the shielded pool is global and token-agnostic |
| Sender signatures / nonces | Phase 3.1: sender Ed25519 sig over `canonical_signature_bytes()`; Phase 5.6: durable `(token_id, sender, seq)` nonce dedup in `PendingTransactionRegistry::used_nonces` | Shielded txs carry a *different* integrity anchor: the proof + nullifiers (the sender identity is hidden). `sequence_number`/`sender` fields on `ShieldedTransaction` are not used for dedup — **nullifier uniqueness replaces nonce dedup** (this is the double-spend defense; see S4.2) |
| 16 MiB RNS resource transfer exists | Yes (Phase: RNS data-plane bridge; 16 MiB ceiling) | Halo2 proof (~1–2 KB) + nullifiers + commitments + note ciphertexts (~2.3 KB each) fit comfortably in one frame; no resource-transfer dependency |
| Pool state implied to ride the block implicitly | `BlockFactory::create_hash` (blocks.rs:132-154) hashes exactly `(previous_hash, timestamp, canonical_signed_trans_bytes, token_metadata, proposer_key, epoch_number)` — `Block` has NO pool-state field | The pool update is hash-bound by riding **inside `signed_trans`**: new additive `SignedTransaction.shielded: Option<ShieldedTransaction>` field (S3.1). Consequences: (a) the finalizer's signature (block_builder.rs:134-170 hashes `rmp(signed_tx)`) and the block hash both bind the exact nullifiers/commitments; (b) pool state becomes a pure function of committed block history — any node can rebuild its view by replaying blocks (S5.1/S5.3 split-deployment views); (c) conflict rollback of a block unwinds exactly that block's hash-bound pool update |
| Finalizer votes were assumed to be "finalizers signing public outputs" generically | Today the **executors** are the voters: `handle_signature` (finalizer.rs:346) authenticates the sender as a registered **Executor** (C1), verifies its inner sig over `sig.transaction_hash`, stamps stake from the epoch snapshot, feeds `SignatureCollector`; first valid sig triggers the optimistic path; quorum is stake-weighted | Shielded flow: **finalizers are the voters** (Executor is skipped). New `handle_sign_shielded`/`handle_shielded_vote` handlers follow the C1 pattern verbatim but authenticate voters as registered **Finalizers**; `SignatureCollector` + stake-weighted quorum math (Phase 6.9 integer u128) reused as-is; no optimistic fast path for shielded (quorum-gated by design — roadmap 2.1) |

### Wire-compat note (ground rule 4) — read once, applies to S3–S5

All wire changes in this plan are **additive**:
- New action strings (confirmed routing, S3.2):
  - `"ShieldedTransfer"` — client → Sentinel, a new **inner** action in the
    sentinel's `on_data_received` match (sentinel.rs:110-124; alongside
    `Process`/`Confirm`/...). In the composite node the outer envelope
    action stays `"Verify"` (the sentinel's only dispatcher-level action)
    with the shielded message as the inner body — same shape as `Process`.
  - `"SignShielded"` — Sentinel → Finalizers (vote request carrying the
    `ShieldedTransaction`); new entry in `FINALIZER_ACTIONS`
    (node_server.rs:43).
  - `"ShieldedVote"` — Finalizer → collector Finalizer (carrying a
    `TransactionSignature` vote); new entry in `FINALIZER_ACTIONS`.
  - **No new committer actions** — the finalizer→committer commit reuses
    `"Commit"`/`"BlockFinalized"`, already `Exact(Finalizer)` in the
    committer's `allowed_senders_for` map (committer.rs:68-78).
  Old nodes see any new action as unknown and **fail closed** (reject,
  never silent accept), per the existing `authenticate_message`
  unknown-action rejection and the dispatcher's `UnknownAction` path.
- `Transaction` struct: unchanged. Shielded transactions use a **new**
  `ShieldedTransaction` struct (S3.1), not new fields on `Transaction` —
  this keeps every existing `#[serde]` shape and every canonical-serialization
  byte sequence (`canonical_signature_bytes`, `canonical_signed_trans_bytes`,
  `Transaction::hash`) identical to today.
- `SignedTransaction` gains ONE additive field:
  `shielded: Option<ShieldedTransaction>` (serde `default` +
  `skip_serializing_if = Option::is_none`). Old decoders skip the unknown
  map key (serde default behavior); for every non-shielded block the field
  is absent, so all existing canonical byte sequences
  (`canonical_signed_trans_bytes`, `Transaction::hash`, block hashes) are
  byte-identical to today. When present, the field's canonical bytes ARE
  added to `canonical_signed_trans_bytes` (and to the finalizer-sig input)
  — that only happens in shielded blocks, which no pre-upgrade node can
  produce or validate.
- New MsgPack payload schemas ride the existing 4-byte-length-prefixed
  framing; no framing change.
- **Rollout order is implicit**: shielded txs are only valid once every
  committer is on a build containing S4/S5; until then the network rejects
  them fail-closed (unknown action / unregistered spec), which is the safe
  direction. No upgrade dance required.

### Error variants (single item, S4.1)

Extend `src/errors.rs` `ValidationFailureReason` (additive enum variants —
serde-tagged, old decoders hitting a new variant is covered by the fail-closed
unknown-action gate, but the variant additions themselves are backward-safe
for new→new): `InvalidShieldedProof`, `UnknownNullifier`, `StaleNullifier`
(spent/replayed), `StaleMerkleRoot`, `InvalidCommitment`,
`ValueBalanceMismatch` (should be caught by the proof; belt-and-suspenders
public check where possible). And a `PneumaticError` variant `Shielded(String)`
for pool/tree/registry-level failures distinct from `Validation(...)`.

---

## Phase S1: Cryptographic primitives (blocks everything else)

All in `src/crypto.rs` (new module or inline, matching the file's section
style) unless noted. Every primitive gets its own item with its own
discriminator. No live proving in unit tests (roadmap Part 5): primitives are
tested directly; the circuit tests in S2 use a stub verifier.

### S1.1 — Integrate Halo2 into the workspace (DONE)
- **Files**: root `Cargo.toml` (workspace deps), new `src/shielded/mod.rs`
  (or `src/shielded.rs`) declaring the shielded module tree, `Cargo.lock`
  (regenerated).
- **Action**: Add `halo2_proofs` (or `halo2curves` + `halo2_gadgets` if we
  want to reuse Orchard's note/commitment gadgets — decide at implementation
  time based on which builds cleanest alongside the existing dep tree;
  prefer the smaller surface first). Confirm `cargo check --workspace` and a
  full `cargo test --workspace` pass with the new deps present (they may
  conflict on `group`/`ff`/`subgroup` versions with `pqcrypto-*`/`ed25519-dalek`).
  Write a minimal smoke test that instantiates a trivial circuit, proves and
  verifies it **once** — gated so it is the only test in the workspace that
  does a live prove (mirrors roadmap Part 5's benchmark-only-live-proving
  rule; keep it in the `shielded` module's test module).
- **Verify**: `cargo check --workspace` clean; workspace suite green;
  discriminator: a test that the trivial circuit's proof verifies and that a
  tampered witness fails.
- **Risk note**: this item can hit dependency-version conflict; if
  `halo2_proofs`'s curve crates clash with anything in the lockfile, resolve
  with `cargo update -p <pkg>` for the smallest possible lockfile delta and
  record it in the checklist entry.

### S1.2 — Poseidon hash in `crypto` (DONE)
- **Files**: `src/crypto.rs` (or `src/shielded/poseidon.rs` re-exported from
  crypto).
- **Action**: Add a Poseidon hash over the curve Halo2 uses (Pasta `pallas`
  base field), alongside `BasicHashProvider` (SHA-256 stays untouched — it is
  used for block hashing and must not change). Expose
  `fn poseidon_hash(inputs: &[Fr]) -> Fr` (or a `PoseidonHashProvider`
  implementing a small local trait — do NOT implement the SHA-256
  `HashProvider` trait, the field types don't match). In-circuit version
  comes in S2; this item is the off-circuit reference implementation.
- **Verify**: known-answer test with a published Poseidon-on-Pasta test
  vector; discriminator: tamper one input → different hash; deterministic
  across calls.

### S1.3 — Note commitment (DONE)
- **Files**: `src/shielded/note.rs` (new).
- **Action**: Define `ShieldedNote { value: u64, owner_pk: [u8;32], rho: Fr, rcm: Fr }`
  and `commit(note) -> Fr` following Orchard's note structure as reference:
  commitment = Poseidon- or Pedersen-style binding of (value, owner_pk, rho)
  with `rcm` as the blinding factor (exact formulation decided by the S1.1
  crate choice; document the formula in the module doc). `owner_pk` is the
  *spend* authorization key (a new 32-byte keypair, Ed25519-style, derived
  from the spend key — NOT the node identity key). Commitment must be
  deterministic given the note fields (rcm is part of the note, not
  randomized per-commit).
- **Verify**: same note → same commitment; any field change → different
  commitment; discriminator: changing `value` by 1 changes the commitment.

### S1.4 — Nullifier derivation (DONE)
- **Files**: `src/shielded/note.rs`.
- **Action**: `nullifier(note, spend_key) -> [u8;32]` = `H(nullifier_domain ||
  spend_key || rho)` (Poseidon or SHA-256 — must be **deterministic and
  unlinkable to the commitment without the spend key**; the `rho` binding is
  what makes two spends of the same note yield the same nullifier, and the
  spend-key mixing is what hides which note). Document the domain byte.
- **Verify**: same (note, spend_key) → same nullifier; different note (diff
  rho) → different nullifier; different spend_key → different nullifier;
  nullifier is not derivable from (commitment, spend_key) alone without rho
  (structural test: no shared components).

### S1.5 — Note encryption (reuse hybrid crypto) (DONE)
- **Files**: `src/shielded/note.rs`, uses `src/crypto.rs`.
- **Action**: `encrypt_note(plaintext: NotePlaintext, recipient: &Ed25519Provider) -> Vec<u8>`
  wrapping `recipient.encrypt_to(recipient.x25519_public_key(),
  recipient.mlkem_public_key(), plaintext)` — the existing hybrid
  `encrypt_to`/`decrypt_from` are reused **verbatim** (they are now PQC
  hybrid, stronger than the roadmap assumed). `NotePlaintext { value,
  memo: Vec<u8>, sender_pk_hint: Option<[u8;32]> }` serde'd to bytes.
  **Viewing key** (roadmap 2.6): a note is additionally encrypted to the
  recipient's *viewing key* (a separate 32-byte key derived at keygen);
  viewing-key holders can decrypt, cannot spend (they lack the spend key).
  Key derivation (spend key → spend auth key + viewing key) is part of the
  prover-crate key model in S3.3; this item just defines the encrypt-to-two-
  recipients helper.
- **Verify**: encrypt→decrypt roundtrip for both spend-key holder and
  viewing-key holder; a spend-key-only holder of a *different* account
  cannot decrypt (fail closed on GCM tag); discriminator: corrupt ciphertext
  → `Err`, never garbage plaintext.
- **Sizing note for S3/S5**: one note ciphertext ≈ 2.3 KB (ML-KEM share) +
  payload; a typical 2-note transfer carries ~4.6 KB.

### S1.6 — Incremental Merkle tree over commitments (DONE)
- **Files**: `src/shielded/tree.rs` (new).
- **Action**: Append-only incremental (binary, fixed depth — start 32,
  parameterized) Merkle tree over `Fr` commitments: `append(commitment) ->
  (new_root, membership_proof(index))`; `verify_proof(commitment, proof,
  root) -> bool`; Poseidon node hashing (S1.2). Efficient: O(log n) root
  update and proof generation. Also: **root serialization** for wire
  (32-byte canonical encoding of the `Fr` root) and a `tree_state` snapshot
  type (root + leaf count + last-appended commitments since last snapshot)
  for S5 committer persistence.
- **Verify**: root changes on append; proof verifies against new root,
  fails against previous root; known-structure test (manually computed
  4-leaf tree matches); discriminator: swap a sibling in a proof → verify
  false; append twice with same commitment → two distinct leaves (dupes
  allowed at the tree level — dedup is the nullifier set's job, S4.2).

**S1 sub-total: ~54h** (matches roadmap; S1.1 is the schedule-risk item —
dependency conflicts or a slow first prove can blow the 4h estimate).

---

## Phase S2: Circuit design — *highest schedule risk in Tier 1*

One combined **Action circuit** (spend + outputs together, Orchard pattern),
NOT separate spend/output circuits (roadmap S2 recommendation).

### S2.1 — Action circuit (DONE)
- **Files**: `src/shielded/circuit.rs`, `src/shielded/circuit_test.rs`
  (test harness per roadmap Part 5: known-good witnesses/proofs, stub
  verifier for unit tests).
- **Action**: Single Halo2 circuit proving, with public inputs (nullifiers,
  new commitments, referenced Merkle root, and the value-balance check):
  1. **Spend**: (a) knowledge of a note opening `(value, owner_pk, rho, rcm)`
     for a commitment at a known index in the tree at the referenced root
     (Merkle path verified in-circuit with S1.2 Poseidon); (b) correct
     nullifier derivation per S1.4; (c) knowledge of spend authority —
     in-circuit check that the spend auth key matches the note's `owner_pk`
     (signature over the nullifier/commitment is done off-circuit as a
     witness relation; the circuit proves the *binding* of keys).
  2. **Outputs**: each new commitment is well-formed (value ≤ max, owner_pk
     is a valid field encoding, rcm ≠ 0 where required).
  3. **Value balance**: sum(input values) = sum(output values) + fee, with
     values hidden inside the commitments (the commitments are
     Pedersen-style in value, so the homomorphic sum check closes in
     circuit; fee is a public constant, configurable, default 0 for v1).
  **Fail-closed circuit construction**: any public input that cannot be
  embedded (root too deep, more nullifiers than circuit capacity) is a
  prover/verifier error, never a skipped constraint.
- **Verify**: happy-path prove+verify (the ONE live-prove test, kept here or
  in S1.1's smoke — decide at impl time, must not run in every unit test);
  negative witnesses: wrong note opening, wrong nullifier derivation, value
  imbalance of 1, commitment not in tree, wrong root → all fail to produce
  a verifying proof (soundness spot-checks, not full soundness proofs);
  discriminator: flip one constraint (comment it out) → a previously-rejected
  invalid witness now verifies, proving the constraint was load-bearing.
- **Budget guard**: roadmap allocates 92h here and says to budget *extra
  review time* rather than compress. If circuit debugging exceeds ~1.5× the
  phase estimate, stop and report back rather than silently extending.

### S2.2 — Verification API (network side) (DONE)
- **Files**: `src/shielded/verify.rs`.
- **Action**: `verify_shielded_proof(proof_bytes, public_inputs) ->
  Result<bool, PneumaticError>` — the ONLY function the node crates call.
  Loads the verifying key (compiled once, cached in `once_cell`), parses
  public inputs, verifies. **Never panics on untrusted input** — malformed
  proof bytes are `Err(Shielded(...))` / `Validation([InvalidShieldedProof])`,
  matching the `verify_sender_signature` idiom (never panics on untrusted
  data). The verifying key is a build-time constant for v1 (Halo2's
  transparent setup derives VK from the circuit; no ceremony).
- **Verify**: valid proof → Ok(true); truncated proof → Err; proof for a
  different root → false/Err; discriminator: corrupt one proof byte →
  rejected, and the rejection is an `Err` or `false`, never a panic or
  silent `true`.

**S2 sub-total: ~92h + review buffer.**

---

## Phase S3: Wire format & transaction types

### S3.1 — `ShieldedTransaction` type (DONE)
- **Files**: `src/transactions.rs` (additive — `Transaction` struct
  unchanged, ground rule 4 respected).
- **Action**:
  ```rust
  #[derive(Serialize, Deserialize, Debug, Clone)]
  pub struct ShieldedTransaction {
      pub id: String,               // unique tx id (uuid, like Transaction)
      pub action: String,           // always "ShieldedTransfer"
      pub token_id: Vec<u8>,        // the self-signed token being transferred
      pub nullifiers: Vec<[u8;32]>, // spent notes' nullifiers (≤ circuit cap)
      pub commitments: Vec<Fr-as-[u8;32]>, // new notes
      pub merkle_root: [u8;32],     // referenced pool root
      pub proof: Vec<u8>,           // Halo2 proof bytes
      pub note_ciphertexts: Vec<Vec<u8>>, // encrypted to recipients (S1.5)
      pub fee: u64,                 // public; 0 for v1
  }
  ```
  Plus `canonical_signature_bytes`-style canonical serializer (BTreeMap-free,
  serde round-trip stable) and `hash()` (SHA-256 over canonical bytes,
  mirroring `Transaction::hash`) — the Finalizer/Committer sign and compare
  these bytes (Phase 3.5's hash-match pattern applies directly).
  **Block carrier (the consensus-binding step)**: `SignedTransaction`
  (transactions.rs:335+ `CanonicalSignedTransaction`, :353-372
  `canonical_signed_trans_bytes`) gains `shielded: Option<ShieldedTransaction>`
  (serde `default` + `skip_serializing_if`); `canonical_signed_trans_bytes`
  appends the canonical shielded bytes when `Some`. A shielded block's
  `SignedTransaction.transaction` holds a **deterministic placeholder**
  `Transaction` — `id` = shielded tx id, `action` = `"ShieldedTransfer"`,
  `token_id` copied, all other fields empty/zero — so the block's hash
  (blocks.rs:132-154, via `canonical_signed_trans_bytes`) binds exactly
  (nullifiers, commitments, referenced root, proof, ciphertexts). This is
  what makes the pool update consensus-bound, replayable from block history,
  and unwound atomically by tip rollback.
- **Verify**: serde roundtrip byte-identical for the canonical form;
  `hash()` stable across deserialization; a non-shielded `SignedTransaction`
  (field absent) produces byte-identical `canonical_signed_trans_bytes` to
  today (regression — run existing blocks.rs hash tests untouched);
  discriminator: field reorder in source → hash unchanged (canonical form
  is struct-field ordered, not map-ordered — same property that made
  `Transaction` canonical safe); discriminator 2: flipping one nullifier
  byte changes the block hash.

### S3.2 — Message wiring (DONE)
- **Files**: `src/messages.rs` (action constants, if any), `sentinel/src/
  sentinel.rs` (`on_data_received` match, :110-124), `finalizer/src/
  finalizer.rs` + `finalizer/src/message_dispatcher.rs` (outbound
  `SignShielded`/`ShieldedVote` helpers), `node-server/src/node_server.rs`
  (`FINALIZER_ACTIONS` :43; finalizer `handle` match :626-659).
- **Action**: Three new action strings, confirmed against the dispatcher:
  - `"ShieldedTransfer"` (client → Sentinel): new arm in the sentinel's
    `on_data_received` inner-action match. In the composite node the outer
    envelope action is still `"Verify"` (sentinel's only dispatcher-level
    action) with the shielded message as inner body — identical shape to
    how `Process` arrives today.
  - `"SignShielded"` (Sentinel → Finalizers): vote request carrying the
    `ShieldedTransaction`; add to `FINALIZER_ACTIONS` and to the finalizer
    plugin's `handle` match (node_server.rs:626-659 currently: `"Sign"` →
    `handle_signature`, else fail-closed).
  - `"ShieldedVote"` (Finalizer → collector Finalizer): carries a
    `TransactionSignature` vote; add to `FINALIZER_ACTIONS` + handle match.
  - **No committer-side changes**: the finalizer→committer commit reuses
    `"Commit"`/`"BlockFinalized"`, already `Exact(Finalizer)` in
    `allowed_senders_for` (committer.rs:68-78) — the fail-closed role→action
    map (Phase 1.3) needs no new entries.
  **Every new entry is fail-closed by default** — an action not in a role's
  `allowed_actions` is rejected by `RoleDispatcher::dispatch`
  (role_dispatcher.rs:139-159, `UnknownAction`), which is the existing
  behavior for unknown actions.
- **Verify**: wire roundtrip of a `Message` carrying `ShieldedTransaction`
  via the existing length-prefixed framing; a message with the new action
  sent to a role that doesn't allow it → rejected (not silently dropped);
  discriminator: remove the role-map entry → the message is rejected
  (proves the gate is load-bearing, not cosmetic).

### S3.3 — `pneumatic_prover` crate

> Expanded client-side, **non-networked** crate below. It *produces* a
> `ShieldedTransaction` (the wire type core defines at
> `transactions.rs:362`) — it never sends anything and never changes the wire.
> See "Wire-compat" — it is ground rule 4, unchanged.

**Grounding — APIs S3.3 reuses (verified in-tree).**

| Surface | Signature (verified) | Used for |
|---|---|---|
| `ShieldedNote` | `struct { value: u64, owner_pk: [u8;32], rho: Fq, rcm: Fq }` (`note.rs:44`) | the in-note model; `Fq` = Pallas scalar |
| `commit` | `fn commit(note: &ShieldedNote) -> EpAffine` (`note.rs:99`) | input & output commitments |
| `nullifier` | `fn nullifier(note: &ShieldedNote, spend_key: &[u8;32]) -> [u8;32]` (`note.rs:143`) | per-input nullifiers (pairwise distinct) |
| `NotePlaintext` | `struct { value, memo, sender_pk_hint }` (`note.rs:158`) | decrypted viewing payload |
| `encrypt_note_to_two` | `note.rs:214` | per-output ciphertexts (spend + viewing) |
| `decrypt_note` | `note.rs:200` | viewing-key scan |
| `ActionCircuit` | `new(note, spend_key:[u8;32], merkle_proof, merkle_root:Fp, output_note, fee:u64, tree_depth)` (`circuit.rs:308`); `public_inputs` (`:352`); `verify_merkle_path_off_circuit` (`:329`) | building/pre-validating the circuit instance |
| `PublicInputs` | `struct { nullifier, commit_x, commit_y, merkle_root, output_commit_x, output_commit_y, fee: Fp }` (`circuit.rs:380`) | the circuit public-input vector |
| `IncrementalMerkleTree` | `new`/`append`/`verify_proof` (`tree.rs`) | Merkle membership |
| `bytes_to_root`/`root_to_bytes` | `bytes_to_root(&[u8;32]) -> Option<Fp>` (fallible) (`tree.rs:324`) | referenced root wire ↔ circuit type |
| `MembershipProof` | `struct { index, siblings }` (`tree.rs:86`) | spend path (`circuit.rs:339`) |
| `ShieldedTransaction` | `transactions.rs:362`; `canonical_bytes` `:399`; `hash` `:408`; `placeholder_transaction` `:421` | the value this crate returns |
| `ShieldedVerifier` | `new` (`verify.rs:111`); `verify(proof, &PublicInputs)` (`:163`) | the crate's own correctness check (test only) |
| Live-prove recipe | `Params::new(K)`; `keygen_pk`; `create_proof(...)` at `circuit_test.rs:327-371`; `K = 10` | how `build_shielded_tx` produces `proof: Vec<u8>` |

**What S3.3 is / is not.** *In scope:* the spend/viewing key model, `create_note`,
`build_shielded_tx`, `scan_for_notes`. *Out of scope:* no network code
(roadmap 2.1 — the network only verifies, the client proves); no circuit design
(`ActionCircuit` is built via core's `shielded::circuit`); no node-side
verification (`ShieldedVerifier` lives in core and is called here only inside the
`#[ignore]`d live-prove test); no wire change.

**Dependencies + workspace mechanics.** Register `prover` as a new workspace
member (root `Cargo.toml` `members`). `halo2_proofs`, `pasta_curves`, `ff`,
`group`, `subtle` are **already production deps of `pneumatic_core`**
(pinned: `halo2_proofs = "=0.3.5"`, `pasta_curves = "=0.5.2"`); `Cargo.lock`
resolves each once. Pinning the prover's direct copies to the **same `=`-versions**
and reusing `rand = "0.8"` (already in the tree) introduces **zero new
lockfile packages** — same zero-delta philosophy the root `Cargo.toml` S1.2–S2.1
comments use. Halo2 items are **not re-exported** by core, so the prover needs
`halo2_proofs` + `pasta_curves` as **direct** deps to name `create_proof`,
`Params`, `Blake2bWrite`, `Fq`/`Fp`.

**Wire-compat note for S3.3 (ground rule 4).** S3.3 produces **no wire change.**
`ShieldedTransaction` is core's (S3.1); the crate only constructs one. No new
action strings, no `Transaction`/`SignedTransaction` field change
(`SignedTransaction.shielded` additive field already exists, populated by the
finalizer, S5.2, from the payload the client submits). The ~1–2 KB proof and
~2.3 KB/note ciphertext (S1/S2 sizing) are already within the 16 MiB frame cap.
Old nodes still fail closed on `"ShieldedTransfer"` (S3.2); the prover has no
bearing on that path.

**Key-model decision (the one real design point).**

- **4a — value-auth key vs. node-identity key.** `owner_pk` (`note.rs:49`) is a
  32-byte **value-authorization** key mixed into the Pedersen commitment; it is
  *not* a node identity key and *not* the `Ed25519Provider` used for encryption
  (`encrypt_note_to_two`, `note.rs:214`, encrypts to a provider's own hybrid
  keys). `SpendKey::from_seed(seed: [u8;32])` derives deterministically:
  `spend_secret` (the `spend_key` passed to `nullifier` + the circuit witness),
  `owner_pk = sha256("pneumatic-shielded/v1/owner-pk" || spend_secret)[..32]`,
  `viewing_key = sha256("pneumatic-shielded/v1/viewing-key" || seed)`. "Ed25519
  keys are fine here" is honored by *allowing* the pair to be an Ed25519
  `(SigningKey, VerifyingKey)`; the default derivation is kept because an
  in-circuit Ed25519 verify is too expensive and the commitment only needs 32
  bytes. The circuit's `owner_pk == H(spend_secret)` binding is **S2.1's**
  in-circuit responsibility (flagged §Open items).
- **4b — encryption recipient vs. `owner_pk`.** v1 bundles each recipient's
  `owner_pk` with its `Ed25519Provider` into a `ShieldedIdentity`; `build_shielded_tx`
  sets `output.note.owner_pk` from the identity and encrypts to its provider.
  Who may hold viewing keys is **policy only** (parent plan Open Q #2); the
  technical path supports both.
- **4c — randomization is per-note only.** `rho`/`rcm` randomized per output at
  `create_note`; no separate note-scoped scheme. Enforced/tested in S3.3.2.

**Corrected vs. the compact bullet (load-bearing).**

1. `scan_for_notes` returns `Vec<NotePlaintext>` **not** `Vec<ShieldedNote>`
   (viewing keys only decrypt the plaintext — `rho`/`rcm` never reach them;
   reconstructing a note would fabricate hidden randomness). Fail-closed:
   a mismatched ciphertext `Err`s.
2. The "one live prove" is gated `#[ignore]`, matching S1/S2/S5/S6 (roadmap Part
   5 — proving is benchmark-only). Default suite tests every cheap property +
   wire assembly from a canonical proof; the single `#[ignore]`d test runs the
   real prove + `ShieldedVerifier::verify`.

**Build items.** The recorded 663 baseline predates **+64 tests merged into the
workspace** since it was written — live-measured now (all crates except the new
`prover`): core 489, sentinel 57, node-server 30, committer 71, finalizer 55,
executor 10, `pipeline_integration` 9, `transport_integration` 6 = **727**. So
this phase tracks from the live `cargo test --workspace` baseline of **727**
(the 663 figure is preserved in the plan's Context as the original snapshot;
S3.3's own progression below is measured, not copied). Every `#[test]` below is
counted, so the progression is **727 → 730 → 734 → 739(+1 ignored) → 741 →
742(+1 ignored)**.

#### S3.3.1 — Crate scaffolding + workspace member + `SpendKey` model

- **Files**: root `Cargo.toml` (`members += "prover"`); `prover/Cargo.toml`;
  `prover/src/lib.rs`; `prover/src/key.rs`.
- **Action**: (1) register `prover` as a workspace member. (2)
  `prover/Cargo.toml`: `package = "pneumatic_prover"`, `edition = "2021"`, deps
  `pneumatic_core = { path = ".." }`, `halo2_proofs = "=0.3.5"`,
  `pasta_curves = "=0.5.2"` (direct — name `Fq`/`Fp`), `rand = "0.8"`,
  `once_cell = "1.21.3"`, `sha2 = "0.10"`, `serde`/`serde_json` (optional) — all
  pinned to versions already resolved → zero new lockfile packages. (3) `key.rs`:
  `SpendKey { seed }` with `from_seed`, `spend_secret()`, `owner_pk()`,
  `viewing_key()`, and `ShieldedIdentity { spend: SpendKey, identity:
  Ed25519Provider }` bundling the value-auth key with the encryption provider
  (§4b). Derivation pinned in code comments.
- **Verify**: `cargo check --workspace` clean (prover compiles against core).
- **Done (discriminator + progression)**: `key_derivation_is_deterministic`
  (same seed → same `spend_secret`/`owner_pk`/`viewing_key`); `changing_seed_changes_all_derived_values`
  (discriminator: change the seed → all three change, proves seed-bound not
  constant); `owner_pk_is_derivable_from_spend_secret` (recomputes
  `owner_pk == H(spend_secret)` — the off-circuit contract the prover satisfies;
  the in-circuit version is S2.1's). **+3 default: 727 → 730.**

#### S3.3.2 — Note creation + ciphertexts (`create_note`)

- **Files**: `prover/src/note_builder.rs`.
- **Action**: `create_note(recipient: &ShieldedIdentity, value: u64) ->
  (ShieldedNote, Vec<u8>)`: randomize `rho`/`rcm` with `rand` (never reused
  within the tx), build `ShieldedNote`, `commit(&note)`, `encrypt_note_to_two`
  the `NotePlaintext` to the spend + viewing providers; return the spend-side
  ciphertext. Input notes are supplied wholesale by the spender; only re-
  randomization applies when re-issuing.
- **Verify**: commitment matches core's `commit`; ciphertext roundtrips.
- **Done (discriminator + progression)**: `create_note_commitment_matches_core`
  (output commitment == `pneumatic_core::shielded::commit(&note)` — proves core's
  tree accepts it at S5.3 append); `create_note_randomizes_rho_and_rcm` (same
  `value`+`owner_pk` → different `rho`/`rcm` → different commitments = unlinkable);
  `note_ciphertext_roundtrip_spender_and_viewing`; `note_ciphertext_wrong_key_fails_closed`
  (foreign provider → `Err`, never garbage — S1.5 discriminator). **+4 default:
  730 → 734.**

#### S3.3.3 — `build_shielded_tx` (wire assembly + prove)

- **Files**: `prover/src/build.rs` (`ProvingKey` holder + `build_shielded_tx`,
  `assemble_tx`), `prover/src/lib.rs`.
- **Action**: `ProvingKey` — `once_cell` `Lazy` building `Params::new(10)` +
  `keygen_pk` **once** (S2.1 recipe; keygen cost stays off the default test path).
  `build_shielded_tx(inputs, spend_keys, merkle_proofs, root, outputs, recipients)
  -> Result<ShieldedTransaction, PneumaticError>`, all fail-closed: pre-validate
  every spend with `verify_merkle_path_off_circuit`; `root` via `bytes_to_root`
  (`None` → `Err`); assemble `ActionCircuit` per input/output; derive nullifiers
  via `nullifier`, assert pairwise distinct; derive output commitments via
  `commit`; encrypt outputs (S3.3.2); `create_proof` → `proof`; populate
  `ShieldedTransaction`. `assemble_tx` wires fields from a canonical proof buffer
  (no live prove) so the assembly/hash/canonical-bytes properties run in the
  default suite.
- **Verify**: well-formed tx, hash-stable; single live-prove test proves+verifies.
- **Done (discriminator + progression)**: `build_shielded_tx_wire_type_is_well_formed`
  (2/2 → 2 nullifiers/commitments/ciphertexts, `action=="ShieldedTransfer"`,
  `merkle_root` is LE(root), `hash()` stable across serde round-trip);
  `build_shielded_tx_value_balance_off_circuit` (`sum(inputs) == sum(outputs)+fee`);
  `build_shielded_tx_nullifier_is_spend_secret_bound` (`nullifier(note, wrong) !=
  nullifier(note, spend)` — structural "wrong spend key" discriminator);
  `merkle_path_off_circuit_accepts_and_rejects`. **[ignore]d live prove**
  `build_shielded_tx_live_prove_verifies`: real `build_shielded_tx` → proof →
  `ShieldedVerifier::verify` `Ok(())`; flip a nullifier → `Err` (fails closed).
  **+5 default (+1 ignored): 734 → 739 (+1 ignored).**

#### S3.3.4 — Viewing-key scan (`scan_for_notes`)

- **Files**: `prover/src/scan.rs`.
- **Action**: `scan_for_notes(ciphertexts, viewing_provider) ->
  Result<Vec<NotePlaintext>, PneumaticError>` — decrypt each supplied ciphertext
  with the viewing key; a non-matching ciphertext `Err`s (fail-closed). Returns
  `NotePlaintext`s (§Correction #1).
- **Verify**: a matching ciphertext decrypts to the right value; wrong viewer →
  `Err`.
- **Done (discriminator + progression)**: `scan_for_notes_returns_plaintext_not_note`
  (asserts result is `NotePlaintext` and no `rho`/`rcm` are recoverable — primary
  discriminator for the audit path); `scan_for_notes_wrong_viewing_key_rejected`.
  **+2 default: 739 → 741.**

#### S3.3.5 — Reuse verify API in the crate's test + integration seam

- **Files**: `prover/src/build.rs` test module (`ShieldedVerifier` from core),
  `prover/src/lib.rs` (re-exports).
- **Action**: confirm the prover's proof shape is exactly what
  `ShieldedVerifier` consumes — same `K=10`, same instance-column order
  (`ShieldedVerifier::instances_for`: nullifier, commit_x, commit_y, merkle_root,
  output_commit_x, output_commit_y, fee — `verify.rs:144`). Restate
  `verify_instances_match_circuit_columns` as a prover-side invariant so a future
  circuit change breaks the prover, not the network.
- **Verify**: `ShieldedVerifier::instances_for(&prover_public_inputs)` equals
  `ActionCircuit::public_inputs()` column order.
- **Done (discriminator + progression)**: `prover_public_inputs_match_verifier_columns`.
  **+1 default: 741 → 742 (+1 ignored).**

**S3.3 verification (this phase).** `cargo check --workspace` clean (no
lockfile growth); `cargo test --workspace` = **742 passed, +1 ignored** (live
prove), up from the live-measured 727 baseline (the recorded 663 predates +64
tests merged into the workspace since it was written); each increment names its
discriminator. The
single `#[ignore]`d live prove runs on demand:
`cargo test -p pneumatic_prover -- --ignored`.

**Risks.** (1) Live-prove cost — `build_shielded_tx` keys off a `Lazy` proving
key; `keygen_pk` is expensive and must stay off the default test path (§S3.3.3
item 3 / Correction #2); slow proving at `K=10` is the S6.4 finding to escalate,
not silently compress here. (2) Halo2 hygiene — exact `=`-pins keep halo2 out of
new lockfile entries; a `cargo update` bumping any of the 5 resolved Halo2/Pasta
crates is an API-migration event (mirror `rns-*` `=`-pin in the root
`Cargo.toml`).

**Open items / flag to roadmap owner.** (1) **Per-output circuit vs. aggregate:**
the S2.1 `ActionCircuit` (`circuit.rs`) is 1-in/1-out; a 2-in/2-out transfer
needs either a wider circuit (an S2.1 circuit-design change, **not** S3.3) or a
documented v1 cap (batch at the wallet layer). Recommend locking the v1
nullifier/commitment cap here and tracking the general multi-output circuit as an
S2.1 follow-up, as S4.1 field bounds / S2.1 circuit capacity imply. (2)
**`owner_pk == H(spend_secret)` binding** is off-circuit here; the soundness of
that binding is the Action circuit's (S2.1) — catch in the S2.1 circuit audit
(parent plan Open Q #1), not closeable in S3.3. (3) **Viewing-key policy /
recipient wiring (§4b):** v1 bundles each recipient's `owner_pk` with its
`Ed25519Provider`; holding policy is tracked in Open Q #2. (4) **Proving benchmark
(S6.4 gating):** the `#[ignore]`d live prove is the S6.4 probe — surface numbers
at S6.4, do not ship S3.3 as the measured proving path.

**S3 sub-total: ~32h.**

---

## Phase S4: Validation layer

### S4.1 — `ShieldedValidationSpec`
- **Files**: `src/validation.rs` (additive), `src/errors.rs` (error
  variants — see Context).
- **Action**: Implement `TransactionValidationSpec` (name `"Shielded"`):
  the trait takes `&Transaction` — the shielded path needs
  `&ShieldedTransaction`. **Resolution**: the spec receives the shielded tx
  via a new trait method with a default impl, OR the pipeline carries both
  types and the spec is registered under the shielded action with a small
  adapter. Decide by reading how `ActionRouter`/sentinel invoke specs; the
  constraint is: `SelfSigned`/`Executed` specs and `register_defaults()` are
  **untouched**, the new spec is registered explicitly (fail-closed: a
  `"Shielded"` tx with no registered spec → `Err`, mirroring Phase 3.2's
  reject-unknown-validator behavior).
  Checks, in order, all fail-closed:
  1. Structural: field bounds (nullifier/commitment counts ≤ circuit
     capacity, non-empty, valid encodings, nullifiers pairwise distinct
     within the tx) → `InvalidCommitment`.
  2. Nullifier set: each nullifier not already in the set → `StaleNullifier`
     (the double-spend check; see S4.2).
  3. Merkle root freshness: `merkle_root` equals the pool's current root OR
     a root within the accepted recency window (S4.3) → `StaleMerkleRoot`.
  4. Proof: `verify_shielded_proof` (S2.2) → `InvalidShieldedProof`.
  `calculate_risk`: shielded txs report a **fixed, neutral** risk factor
  (amount unknown by design — use `affected_parties: 2, amount: 0`); the
  environment's `max_risk` gate still applies to the fixed value.
  Register via `ValidationSpecRegistry`/`BlockValidatorSpecRegistry` in the
  same places `register_defaults()` runs (but as a separate
  `register_shielded()` — do not put it in `register_defaults`, so a
  non-shielded deployment doesn't pay for the verifying key).
- **Verify**: each of the four checks has its own rejection test with the
  exact `ValidationFailureReason`; a fully valid shielded tx passes;
  discriminator per check: e.g., pre-insert the nullifier into the set →
  `StaleNullifier` (proves check 2 runs, not just check 4).

### S4.2 — Nullifier-set registry
- **Files**: `src/registry.rs` (new struct alongside `PendingTransactionRegistry`).
- **Action**: `NullifierRegistry` — DashMap-based, mirroring the
  `PendingTransactionRegistry` conventions (every method `Result`, atomic
  `insert`-returning-old-value for the check-or-insert so there is **no
  TOCTOU window** — the same fix `add_transaction` and `used_nonces` got):
  `try_mark_spent(nullifier) -> Result<(), PneumaticError>` (Err
  `StaleNullifier` if present), `contains(nullifier)`, `mark_many_atomic(
  &[nullifier])` (all-or-nothing: check all, then insert all — a tx with 2
  nullifiers must not spend one and reject on the other), plus the
  `used_nonces`-style "never evicted" comment: **nullifiers are never
  removed** (roadmap 2.5: a rollback that un-spends a nullifier is a
  double-spend). Growth handling: v1 is unbounded (documented; the set is
  32 B/spend — 10M spends ≈ 320 MB, flag as a future compaction item, NOT
  built now).
- **Verify**: double-mark rejected; atomic batch: 2nd nullifier already
  spent → 1st not marked (all-or-nothing discriminator); concurrent
  `std::thread::spawn` + `Arc` stress test (N threads, 1 nullifier →
  exactly 1 success — mirrors `concurrent_add_signature_same_executor_one_
  succeeds`).

### S4.3 — Merkle-root freshness
- **Files**: `src/shielded/` (root state), `src/validation.rs` (the check).
- **Action**: The pool root only advances as commitments land in committed
  blocks (S5.3). A shielded tx references the root it was proved against.
  Accept if `referenced_root == current_root`; also accept roots within the
  last K committed pool states (K configurable, default small, e.g. 10 —
  mirrors the orphan-buffer's tolerance for near-tip blocks, Phase 3.4
  pattern). Older → `StaleMerkleRoot`, fail-closed.
- **Verify**: current root accepted; root from K-1 states back accepted;
  K+1 back rejected; discriminator: set K=0 in a test → the K-1 case now
  rejects (proves the window logic, not just equality).

**S4 sub-total: ~28h.**

---

## Phase S5: Pipeline rewiring

Touch points confirmed against the code (Explore pass): sentinel inner-action
match at sentinel.rs:110-124; finalizer voting at `handle_signature`
(finalizer.rs:346) + `BlockBuilder::sign_finalizer_block` (block_builder.rs:
134-170); committer commit at `handle_commit` (committer.rs:432) →
`check_and_commit_transaction_results` (:461, H12 hash-match at :519,
conflict+commit at :527-535) and the `handle_block_finalized` append path
(:672); composite wiring at node_server.rs:36-43 (action constants),
:204-402 (`build_runtime` DI bundle), :409-520 (`build_role_plugin`).
Structural fact driving S5.3: the block hash binds only
`(previous_hash, timestamp, signed_trans, metadata, proposer, epoch)`, so
the pool update is hash-bound by living in `signed_trans.shielded` (S3.1).

### S5.1 — Sentinel: shielded routing branch
- **Files**: `sentinel/src/sentinel.rs` (inner-action match `on_data_received`
  :110-124 — new `"ShieldedTransfer"` arm; new `handle_shielded_transfer`),
  `src/registry.rs` (parallel shielded map, see Action).
- **Action**: Third routing branch alongside SelfSigned (→ Committer direct)
  and Standard (→ Executor → Finalizer): **shielded → proof verification
  (S4.1) → Finalizer directly, bypassing Executor/preload entirely**
  (roadmap 2.1). `handle_shielded_transfer`:
  1. Deserialize `ShieldedTransaction` from the inner body (encoding error
     → fail-closed `Encoding`).
  2. Run `ShieldedValidationSpec` (S4.1) — **advisory pre-check**: it
     rejects obvious garbage (bad proof, already-spent nullifier, stale
     root) before any finalizer work, but is NOT the consensus gate — the
     authoritative re-check is the committer's guarded apply (S5.3).
  3. Token lookup: `token_id` must reference an existing, `is_self_verified`,
     shielded-opt-in token (fail-closed `TokenNotFound` / `NotSelfVerified`).
     The sentinel does NOT fetch token data for the *amount* — it has no
     balance data to check and none is revealed.
  4. Register in a new parallel map on `PendingTransactionRegistry`:
     `shielded_transactions: DashMap<String, ShieldedTransaction>`
     (mirrors the `used_nonces` side-map pattern; never evicted — the
     committer's H12-style hash match, S5.3, compares against it). No new
     `TransactionState` variant: shielded txs are not `Transaction`s and do
     not ride the `Pending → … → Committed` state machine.
  5. Send `Message::signed("SignShielded", shielded_tx_bytes)` to the
     finalizer set — the deterministic finalizer-assignment machinery
     (TASKS Phase 3) picks the collector, same as standard txs.
  **Pool view**: the sentinel needs the pool's nullifier set + root history
  for step 2. Design: ONE authoritative `ShieldedPool` (S5.3) is the
  committer's structure; in the **composite node** (primary deployment —
  node-server hosts all roles) the sentinel/finalizer/committer share one
  `Arc<ShieldedPool>` from the `build_runtime` DI bundle (sentinel and
  finalizer read-only). In **split deployments** the pool state is a pure
  function of committed block history (S3.1: the update is hash-bound
  inside `signed_trans.shielded`), so a sentinel/finalizer rebuilds and
  maintains its local view by replaying the shielded update of every
  received distributed/finalized block — deterministic, no extra sync
  message, and the K-recency window (S4.3) absorbs view lag.
- **Verify**: shielded tx reaches Finalizer without Executor dispatch
  (assert no executor message sent); non-shielded paths unchanged
  (regression: existing sentinel routing tests all pass untouched);
  discriminator: point a shielded tx at a non-self-verified token →
  rejected fail-closed.

### S5.2 — Finalizer: sign over public outputs
- **Files**: `finalizer/src/finalizer.rs` (new `handle_sign_shielded` +
  `handle_shielded_vote`, siblings of `handle_signature` :346),
  `finalizer/src/signature_collector.rs`, `finalizer/src/block_builder.rs`
  (shielded variant of `build_signed_transaction` :78-125;
  `sign_finalizer_block` :134-170), `finalizer/src/message_dispatcher.rs`
  (outbound `ShieldedVote` fan-out to peers).
- **Action**: Two new handlers on the finalizer, following the
  `handle_signature` C1 pattern (finalizer.rs:346) verbatim but
  authenticating the sender as a registered **Finalizer** (not Executor —
  the Executor stage is skipped for shielded txs):
  - `handle_sign_shielded` (vote request, from the sentinel):
    1. Authenticate the envelope sender as a registered Finalizer (C1:
       envelope sig + registry lookup) — unknown voter → reject,
       fail-closed.
    2. **Re-run `verify_shielded_proof` (S2.2) over the tx's own public
       inputs** — each finalizer verifies independently; the sentinel's
       verdict is never relayed (a compromised sentinel cannot forge
       quorum — Phase 1.3/1.4 authenticate-don't-trust). Verification
       fails → reject, no vote (fail-closed).
    3. Local pool view must accept the tx's referenced root (K-recency,
       S4.3) — stale locally → reject, no vote.
    4. If this finalizer is the assigned collector: record the tx and fan
       out `Message::signed("ShieldedVote", ...)` to the other finalizers
       (each performs the same re-verify-and-vote procedure).
  - `handle_shielded_vote` (vote, to the collector): authenticate the
    sender as a registered Finalizer (C1), verify the inner vote signature
    (the Phase 1.4 discipline — negated verify result checked, no bare
    `?` on the verify boolean), stamp `current_stake` from the epoch
    snapshot (never self-reported), feed `SignatureCollector` — reused
    as-is.
  **What a finalizer signs**: Ed25519 over `ShieldedTransaction::hash()`
  (SHA-256 of the canonical shielded bytes, S3.1 — binds nullifiers,
  commitments, referenced (pre) root, proof, ciphertexts, tx id). The
  proof-valid boolean is **not** part of the signature input: it is a pure
  function of the tx's public inputs, so any node — notably the committer
  (S5.3, step 0) — recomputes it instead of trusting a voter. The vote's
  job is identity + exact-bytes binding + quorum weight, nothing more.
  Carried in a `TransactionSignature { transaction_hash:
  ShieldedTransaction::hash(), signature, current_stake }` — no new wire
  struct. The vote binds the **referenced (pre) root, not a post root**:
  the post-commit pool root is computed by the Committer at append time
  (S5.3) — signing a value no voter can independently recompute would be a
  consensus hole. Quorum math (stake-weighted, integer u128 per Phase 6.9)
  reused as-is; **no optimistic fast path** — the `signature_count == 1`
  branch of `try_finalize_optimistic` (finalizer.rs:512) does not apply; a
  shielded tx commits only at stake-weighted quorum (roadmap 2.1:
  quorum-gated by design).
  **On quorum** (collector finalizer, mirroring the `try_finalize` tail,
  finalizer.rs:415-494): build the `SignedTransaction` with the S3.1
  placeholder `Transaction` + `shielded: Some(tx)` + the collected votes in
  `executor_sigs` (keyed by voting finalizer public keys — the map is the
  canonical voter-sig carrier in shielded blocks; the field name is
  historical. A dedicated `finalizer_votes` field was considered and
  rejected to keep the canonical schema change to exactly one additive
  field) → `sign_finalizer_block` (block_builder.rs:134-170) — its formula
  `SHA256(SHA256(rmp(signed_tx)) || SHA256(sorted-concat sigs))` is
  **unchanged**: `rmp(signed_tx)` already covers the `shielded` field and
  the sig-concat covers the votes (sorted by key, as today) →
  `create_block` → `TransactionCommit` → `send_to_committers` +
  `send_block_finalized` (existing tail, finalizer.rs:476-494).
  **Impl-time check**: confirm the `SelfSigned` block-validation spec
  (the spec shielded-opt-in tokens route through, Phase 5.9) does not
  require non-empty `executor_sigs`; if it does, gate that check on
  `signed_trans.shielded.is_none()` (additive, fail-closed guard).
- **Verify**: quorum reached → commit message carries the signed public
  outputs (placeholder + shielded field + votes); below-quorum → no commit
  (existing behavior); discriminator 1: mutate one commitment in the signed
  set after signatures are collected → `sign_finalizer_block` hash mismatch
  → rejected (proves the signature binds the exact bytes); discriminator 2:
  a finalizer that receives `SignShielded` for a tx it cannot verify (bad
  proof, or root stale against its local view) sends no vote → quorum
  unreachable in the test → no commit (proves each voter's own check gates
  its vote).

### S5.3 — Committer: pool state persistence + nullifier commit
- **Files**: `committer/src/committer.rs` (`handle_commit` :432 →
  `check_and_commit_transaction_results` :461 — H12 extension at :519,
  conflict+commit dispatch :527-535; `handle_block_finalized` :672 —
  second hook site), `committer/src/block_services.rs` (`commit_block`
  :67-105 — commit-path hook site), `committer/src/epoch_manager.rs`
  (only if the pool state rides the epoch snapshots — Phase 5.4's
  attested-snapshot pattern with SHA-256 envelopes is the model to copy),
  `src/data.rs` (new `DataOp`s for pool-state load/save).
- **Action**: **Cross-token ordering** (the design point the roadmap leaves
  implicit — blocks are per-token lattice chains, the pool is global, so
  there is no inherent global block order): the pool is ONE global
  committer structure behind a **single-writer guard** (the Phase 5.4
  "single epoch writer" pattern — one lock, no concurrent root advances).
  It holds (a) the Merkle tree (S1.6), (b) the nullifier set (S4.2), and
  (c) an **applied-update map** `(block_hash → {leaves appended, nullifiers
  marked})` — the per-block delta that makes replay idempotent and rollback
  exact. A shielded block carries `(referenced_root, commitments,
  nullifiers)`; at commit the guarded `apply_pool_update(block)` runs, in
  order:
  0. **Re-run `verify_shielded_proof` (S2.2) over the block's public
     inputs** — the committer's own check is the final, authoritative
     consensus gate; the sentinel pre-check (S5.1) and the finalizer votes
     (S5.2) are availability/advisory layers, not soundness layers. Proof
     invalid → reject the commit (deterministic on every committer).
  1. **Idempotency**: if `block.current_hash` is already in the
     applied-update map → **no-op** (legitimate replay: a block can reach
     the pool via both the `Commit` path and `BlockFinalized`, and via
     re-gossip). This no-op is what makes double-delivery safe.
  2. **Double-spend**: else if ANY of the block's nullifiers is already in
     the set (which, given step 1, means it was marked by a DIFFERENT
     block) → reject `StaleNullifier` (commit error, fail-closed). Steps
     1+2 together are what make the apply both idempotent AND
     first-spent-wins.
  3. **Freshness**: `referenced_root` within the K-recency window (S4.3).
     K is a **consensus parameter** — it lives in the environment spec and
     every node must run the same value; the window absorbs per-token
     chain lag when blocks interleave and split-deployment view lag.
  4. **Apply**: `mark_many_atomic` (S4.2, all-or-nothing) + append
     commitments + compute the post root + record `(block_hash, delta)` in
     the applied-update map — atomically with the chain append, under the
     same guard. The block does NOT assert a post root; the committer
     computes it (consistent with S5.2 — finalizers sign the referenced
     root only). First-spent-wins is global by construction:
     `mark_many_atomic`'s all-or-nothing check runs under the pool guard,
     so two shielded blocks (even on different tokens) claiming the same
     note can never both pass.
  **Hook sites** (both must call `apply_pool_update` under the guard,
  before the block is reported committed): the commit path —
  `check_and_commit_transaction_results` → `block_services.commit_block`
  (committer.rs:529-532, block_services.rs:67-105) — and the
  `handle_block_finalized` append path (committer.rs:672, Phase 3.3's
  `append_validated_block`).
  **H12 extension**: alongside the existing hash-match at committer.rs:519
  (`transaction.hash() !=` wire tx `hash()` → `TransactionPayloadMismatch`),
  a shielded commit additionally hash-matches the wire
  `signed_trans.shielded` against the validated registry entry (S5.1's
  `shielded_transactions` map) — mismatch → same
  `TransactionPayloadMismatch` error.
  **Conflict/rollback**: pool state is applied as part of the commit unit,
  so when `resolve_block_conflict()` rolls a block back (Phase 5.2's
  tip-rollback path), the pool update unwinds in the same code path and
  under the same guard — inverse op: pop this block's appended leaves,
  remove exactly this block's nullifiers (a later block's spends of notes
  introduced by the loser are inherently invalidated by the root rewind,
  the same way chain rollback invalidates branch builds today — expected
  UTXO semantics, not a bug to solve).
  Persistence via the data provider (`SaveOp`/`GetOp`), with the Phase 5.4
  snapshot-integrity pattern (hash-verified envelope, persist errors
  surfaced, not swallowed). **Durability ordering** (roadmap 2.5):
  nullifiers are marked durable BEFORE the block is reported committed to
  peers — a node that reports commit must have the nullifiers durably
  written, else a crash-and-restart could un-spend. Load at boot:
  missing/corrupt pool state → fail closed (boot error, like Phase 6.5's
  spec-load behavior), never "start empty."
- **Verify**: commit → tree root advances, nullifiers marked, persisted;
  restart → identical pool state (roundtrip test); mismatched wire payload
  → rejected with the payload-mismatch error; **idempotency**: applying
  the same block twice (Commit, then BlockFinalized) → second apply is a
  no-op, root unchanged, nullifiers marked exactly once; **cross-block
  double-spend**: two distinct blocks spending the same nullifier → first
  commits, second rejected `StaleNullifier`; **authoritative re-check**: a
  tx with a full finalizer quorum but an invalid proof → committer still
  rejects (proves soundness does not depend on trusting voters);
  discriminator: skip the durability ordering (mark nullifiers after
  reporting commit) in a test simulating crash-after-report →
  double-spend window opens (the test exists to PROVE the ordering
  matters, and the real code is then shown to close it).
- **Epoch check** (roadmap S5.4): confirm `resolve_block_conflict()` and
  epoch-boundary logic behave correctly when blocks carry shielded pool
  updates — verify with a targeted test, don't assume. The conflict
  machinery itself needs no epoch-logic changes (pool updates ride inside
  the block; the single-writer guard serializes them); what IS new and must
  be tested: the rollback path unwinds pool state in lockstep with
  `blockchain.remove_block()` — add a test where a shielded block loses a
  conflict and assert: loser's commitments gone from the tree, loser's
  nullifiers unmarked, winner's update intact, root consistent.

### S5.4 — Node-server role wiring
- **Files**: `node-server/src/node_server.rs` (action constants :36-43,
  finalizer `handle` match :626-659, `build_runtime` :204-402,
  `build_role_plugin` :409-520); `node-server/src/role_dispatcher.rs`
  (UNCHANGED — listed because its fail-closed `dispatch` :139-159 is what
  the regression test pins).
- **Action**:
  1. `FINALIZER_ACTIONS` (node_server.rs:43, currently `&["Sign",
     "Finalize"]`) += `"SignShielded"`, `"ShieldedVote"`; the finalizer
     `handle` match (:626-659) gains `"SignShielded"` →
     `handle_sign_shielded` and `"ShieldedVote"` → `handle_shielded_vote`
     (S5.2); `"Sign"` → `handle_signature` and the else-fail-closed arm
     are untouched.
  2. `SENTINEL_ACTIONS` (`&["Verify"]`, :41) and `COMMITTER_ACTIONS`
     (`&["Commit"]`, :37) are **unchanged** — shielded transfers arrive
     as an inner action under the `Verify` envelope (S3.2), and shielded
     commits ride the existing `Commit`/`BlockFinalized` actions whose
     auth is already `Exact(Finalizer)` in `allowed_senders_for`
     (committer.rs:68-78) — no new committer map entries.
  3. `build_runtime` (:204-402): construct the `ShieldedPool` (S1.6 tree +
     S4.2 nullifier registry + applied-update map, under the
     single-writer guard) as a shared `Arc` in the DI bundle alongside
     `tokens` (:295) / `pending_registry` (:296); `build_role_plugin`
     (:409-520) threads it into the Sentinel plugin (read-only view for
     S5.1 step 2), the Finalizer plugin (read-only, for the K-recency
     check in S5.2), and the Committer plugin / `BlockServices` (write
     path, S5.3).
  4. `route_data_plane` (:531-538): no special-casing — shielded payloads
     are ordinary length-prefixed messages (size note: ~4.6 KB for a
     typical 2-in/2-out transfer with PQC-hybrid note ciphertexts, well
     under the 16 MiB frame cap).
- **Verify**: composite node (all roles) routes a shielded tx end-to-end in
  the in-process pipeline test; a role receiving an action outside its set
  → rejected (fail-closed regression preserved); discriminator: remove
  `"SignShielded"` from `FINALIZER_ACTIONS` in a test build → vote
  requests rejected by `RoleDispatcher` (`UnknownAction`) — proves the
  gate is load-bearing.

**S5 sub-total: ~48h.**

---

## Phase S6: Testing & hardening

### S6.1 — End-to-end integration test
- **Files**: `tests/shielded_pipeline.rs` (new integration test, following
  `committer/tests/pipeline_integration.rs` fixture conventions).
- **Action**: Full pipeline in-process: prover crate builds a real proof
  (the ONE live-prove integration test — allowed to be slow) → Sentinel
  verifies + routes → Finalizer quorum over public outputs → Committer
  appends block, updates tree + nullifier set → assert: balance conservation
  (sum of outputs = inputs, via decrypting with viewing keys), commitments
  in tree, nullifiers marked, observer-visible surface contains ONLY
  (nullifiers, commitments, root, proof, hash) — **no sender/receiver/amount
  bytes anywhere on the wire** (assert by scanning the serialized messages).
- **Verify**: test passes; the privacy assertion (no plaintext on wire) is
  the headline discriminator — a regression that leaks a field fails it.

### S6.2 — Double-spend & adversarial tests
- **Files**: `tests/shielded_attacks.rs` (or in-crate test modules per the
  item's location).
- **Action**: (a) replay a spent nullifier → `StaleNullifier`; (b) two
  concurrent txs spending the same note (same nullifier) → exactly one
  commits; (c) proof against a stale root (beyond K) → `StaleMerkleRoot`;
  (d) malformed/truncated proof → `Err`, no panic; (e) value-imbalance
  witness (if S2 produced a test vector for it) → rejected; (f) shielded
  tx to a non-shielded token → rejected.
- **Verify**: all six rejected with the exact named errors; discriminator
  for (b): the concurrency test asserting exactly-one-commit is itself the
  discriminator (a TOCTOU in `mark_many_atomic` fails it).

### S6.3 — Concurrency stress
- **Files**: `src/registry.rs` (nullifier registry stress), `src/shielded/
  tree.rs` (concurrent read-while-append).
- **Action**: `std::thread::spawn` + `Arc` per the `registry.rs`
  conventions: 50 threads hammering `try_mark_spent` on mixed unique/dup
  nullifiers → exactly the unique ones marked; concurrent
  `verify_proof` + `append` on the tree → no data race (also run under
  `cargo test` with `--release` once, and if Miri is feasible on the small
  tree test, add it as a documented follow-up, not a gate).
- **Verify**: stress assertions hold; no panics (same standard as
  `concurrent_acquire_release_stress_50`).

### S6.4 — Proving/verification benchmarks
- **Files**: `prover/benches/` or `tests/shielded_bench.rs` (bench-gated,
  excluded from the default suite run like the live-prove tests).
- **Action**: Measure client-side proving time and network-side verification
  time at the final circuit size; record both in the checklist entry and
  **report back to the roadmap owner** (roadmap Part 7: confirming seconds-
  scale proving is acceptable for wallet UX is an open product question).
- **Verify**: numbers recorded; verification time is asserted to be < 100ms
  (network-side budget — if the circuit blows it, that's a circuit-design
  finding to escalate, not a silent accept).

**S6 sub-total: ~46h.**

---

## Total & sequencing

**~300h** (matches the roadmap's Tier 1 estimate). Parallelizable: S1.2–S1.6
once S1.1 lands; S3.1–S3.2 in parallel with S2 (types don't need the
circuit, only S3.3's `build_shielded_tx` and S6.1 do); S4 after S2.2; S5
after S3+S4; S6 last, with S6.1 gated on a working S3.3.

**Hard boundary (roadmap Part 0)**: if this work expands toward hiding
contract logic/state (Tier 2), stop and report — that is a separate
research initiative (zk-VM territory), not an extension of this plan.

## Open questions to flag to the roadmap owner (roadmap Part 7 — NOT
decided here)

1. **Circuit audit**: before this touches real value, the Action circuit
   needs external review/audit (zk constraint bugs are a different risk
   class from Rust bugs — silently-accepting invalid proofs).
2. **Viewing-key policy**: scope is DECIDED — full in v1 (confirmed with
   the product owner). Remaining: who may hold viewing keys (owner-only vs.
   compliance parties) is a policy decision after ship — the technical path
   (S1.5/S3.3) supports both.
3. **Proving UX**: S6.4's measured prove time determines whether client-
   side proving is acceptable on the intended hardware class.
4. **Anonymity-set bootstrap**: the shared pool only provides real privacy
   with real usage — an adoption concern to surface, not an engineering
   blocker.

## Verification (whole-plan)

- After **every item**: `cargo check --workspace` + `cargo test --workspace`
  green, test count monotonically increasing from the current live baseline
  (727 — the recorded 663 in Context is the plan-writing snapshot; the workspace
  has since gained +64 tests), each increment named with its discriminator in the
  AUDIT_CHECKLIST-format
  "Done" note.
- Phase gates: S1 done = primitives unit-tested, no live prove in default
  suite. S2 done = circuit + test vectors + stub-verifier unit tests green.
  S3 done = wire roundtrips + prover crate builds a verifying tx (one live
  prove). S4 done = all four fail-closed checks individually demonstrated.
  S5 done = in-process pipeline moves a shielded tx end-to-end. S6 done =
  S6.1–S6.4 green, benchmarks recorded, open questions reported.
- Final: `cargo test --workspace` full run, record final count in
  AUDIT_CHECKLIST.md following the existing phase-entry format, with the
  wire-compat note from the Context section reproduced in the entry.
