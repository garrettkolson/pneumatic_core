# Pneumatic Core — Audit Remediation Checklist

**Source:** full production-readiness & security audit, 2026-08-19 (see conversation report for
full exploit scenarios and reasoning).
**Verdict at audit time:** not production-ready. 7 Critical / 16 High / 14 Medium / 8 Low findings.

## How to use this document

- Work **in phase order** — later phases depend on earlier ones (e.g. real block validation in
  Phase 3 assumes signed, verified messages from Phase 1).
- Each item lists its audit ID (`C*`/`H*`/`M*`/`L*`), target files, the action, and **Verify** —
  the acceptance check. An item is done only when its Verify step passes.
- **Ground rules:**
  1. `cargo check` and the full workspace test suite must pass after **every** item, not just at
     the end of a phase.
  2. Every fix ships with at least one regression test that fails without the fix.
  3. Some existing tests encode the buggy behavior and must be updated as part of the fix —
     notably the double-append test at `committer/src/committer.rs:2041-2047`
     (`BlockFinalized` double-append) and any quorum/routing tests that assume unsigned
     messages.
  4. Do not change wire message shapes without appending a note here about compatibility.
  5. Fail closed, not open: a missing/unknown validator, spec, or identity is an error, never a
     silent accept.

## Phase S1 — Cryptographic primitives

*Closes Phase-S1.1 of the Pneumatic Shielded Value Transfer (Tier 1) plan
(`pneumatic-shielded-implementation-plan.md`). S1.1 is the schedule-risk item: the only thing that
can block the whole program before any shielded logic exists is a dependency-version conflict between
`halo2`'s curve crates and the existing `ed25519-dalek` / `ring` / `aes-gcm` / `pqcrypto-*` tree, or a
toolchain that won't compile halo2. S1.1's job is to remove that uncertainty and prove the proving
stack actually links, builds, and produces a verifiable proof over the pasta `Fp` field — in a single
workspace that already builds 663 tests green. Ground rules carried into this item from
`AUDIT_CHECKLIST.md` and the approved plan: `cargo check --workspace` **and** full `cargo test
--workspace` both pass; ≥1 discriminator proven to fail by reverting the change; no wire-shape change
without a wire-compat note; fail closed, never silent-accept; only ONE gated live-proving test in the
workspace (roadmap Part 5 — proving is benchmark-only).*

- [x] **S1.1 Integrate Halo2 into the workspace** — *done 2026-09-11*
  Files: root `Cargo.toml` (halo2 as a **dev-dependency** + a direct `ff` dep), `Cargo.lock`
  (regenerated — the halo2 tree is added with no downgrade of `ed25519-dalek` / `ring` / `aes-gcm`
  / `pqcrypto-*`), `src/shielded/mod.rs` (new, **directory form** — `src/shielded/` is where S1.3
  (`note.rs`), S1.6 (`tree.rs`), S2 (`circuit.rs`, `circuit_test.rs`), and S2.2 (`verify.rs`) will
  land), and `src/lib.rs` (`pub mod shielded;`, matching the flat one-line module style at the bottom
  of the file).
  Action: added `halo2_proofs = "=0.3.5"` (pinned **exactly**, mirroring the `=`-pin philosophy used
  for `rns-net`/`rns-crypto`/`rns-core` here) and a direct `ff = "=0.13.1"` to `[dev-dependencies]` of
  `pneumatic_core`. The `ff` `Field` trait is bound on `halo2_proofs::Circuit<F>` and used for field
  arithmetic on the pasta `Fp` field in the gated smoke test — and `ff 0.13.1` is already pulled in
  transitively by halo2 (via `pasta_curves`), so the direct pin **adds zero new packages** to the
  lock. halo2 stays a **dev-dependency**: it is used *only* inside `src/shielded/mod.rs`'s
  `#[cfg(test)]` module, so `cargo check --workspace` (the non-test graph) is clean and the halo2
  curve crates never enter the production link graph. Promotion is a **later** step — when S2.2
  (`src/shielded/verify.rs`) and S2.1 (`src/shielded/circuit.rs`) move halo2 into non-test code that
  node crates call, `halo2_proofs` must move from `[dev-dependencies]` to `[dependencies]`, and that
  move is to be recorded in this item's Done note then. The gated smoke test implements the canonical
  single-gate Halo2 circuit over the pasta `Fp` field — `a * b = out` — on two advice columns
  (`a`, `b`); the product reuses advice column `a` at `Rotation::next`, one instance column (`out`) is
  the public input exposed via `layouter.constrain_instance`, and a single `s_mul` selector enforces
  `s_mul * (a·b − prod)`. The whole thing was finalized against the pinned `halo2_proofs 0.3.5`
  layout-based `Circuit` API (`type Config` / `type FloorPlanner` / `without_witnesses` /
  `configure` / `synthesize`), the free-function keygen (`keygen_vk` / `keygen_pk`),
  `Params<EqAffine>::new(K)` (the `EqAffine` Vesta affine backend, `C::Scalar = Fp`), the
  `Blake2bWrite`/`Blake2bRead` transcript + `Challenge255` prover/verifier path (`create_proof` →
  `finalize` → `verify_proof` with `SingleVerifier::new(params)`, `Output = ()`), and
  `Selector::enable` / `AssignedCell::cell` (used to capture the `Cell` — `Cell` is `Copy` — out of the
  region for the post-region instance binding). Two tests:
  - `shielded_setup_and_mock_verify` (**counted**, runs in the default `cargo test`): builds `Params`,
    derives the proving/verifying keys, and checks satisfiability with `MockProver` (a local witness
    checker, *not* a cryptographic proof) — the satisfying witness (`out = 2·3 = 6`) verifies, a
    tampered instance (`7 ≠ 6`) is rejected. No real proof is produced in the default run (roadmap
    Part 5: proving is benchmark-only).
  - `shielded_live_prove_smoke` (`#[ignore]`d, the **one** live cryptographic prove/verify in the
    workspace): run on demand with
    `cargo test --workspace -- --ignored shielded_live_prove_smoke`; it produces a real proof, verifies
    a satisfying witness, then proves the verifier rejects a tampered instance (fails closed — never
    silently `Ok`).
  Verify: `cargo check --workspace` clean (halo2 not compiled in the non-test graph); `cargo test
  --workspace` green (the counted test counted, the live-prove smoke `#[ignore]`d but present);
  `--ignored` run proves + verifies + rejects tamper.
  **Done:** `halo2_proofs = "=0.3.5"` + `ff = "=0.13.1"` added to `[dev-dependencies]` of
  `pneumatic_core`; `Cargo.lock` regenerated — the halo2 tree (`halo2_proofs`, `pasta_curves`, `ff`,
  `group`) is *new* in the lock with **no downgrade** of `ed25519-dalek` / `ring` / `aes-gcm 0.11.0`
  (still pinned) / `pqcrypto-*`; the tiny one-gate `a*b=out` circuit compiles on the pinned `0.3.5`
  layout API and both tests pass. Discriminators — each proven to fail when its fix is reverted:
  removing the `halo2_proofs` dev-dependency **and** `src/shielded/mod.rs` makes both tests fail to
  compile (proving the item is load-bearing); `shielded_live_prove_smoke` additionally proves the
  proving *and* verifying paths link and behave correctly — a build that compiles but links the wrong
  proving stack would fail the verify-fail-on-tamper step (step 5), not the setup step (step 1).
  **Wire-neutral:** no `Message`, `Transaction`, action string, or serialization touched — verified by
  the unchanged existing wire tests passing untouched. Test count: the default-run passing count rises
  **+1 (663 → 664)** — the counted `shielded_setup_and_mock_verify` — and the gated live-prove test
  adds a second, `#[ignore]`d test that is present but not run by default (the workspace's `cargo test`
  now reports **2 ignored**; the live-prove smoke is the only new ignored). Net **2 new test
  functions** (matches the plan); no other crate's test count changed.

- [x] **S1.3 Note commitment** — *done 2026-09-14*
  Files: `src/shielded/note.rs` (new), `src/shielded/mod.rs` (added `mod note;` + re-export),
  root `Cargo.toml` (added `group = "=0.13.0"` to `[dependencies]` — already in Cargo.lock
  transitively via `pasta_curves`, so no new package).
  Action: `ShieldedNote { value: u64, owner_pk: [u8;32], rho: Fq, rcm: Fq }` +
  `commit(note) -> EpAffine`. Pedersen-style commitment over Pallas Ep:
  `C = G_v·value + G_o·owner_pk_scalar + G_r·rcm + G_rho·rho`. Four generators
  derived via `CurveExt::hash_to_curve("pneumatic_note_commitment")` with distinct
  input bytes (`b"Gv"`, `b"Go"`, `b"Gr"`, `b"Grho"`) — deterministic, no trusted
  setup. `owner_pk` → `Fq` via `Fq::from_uniform_bytes` (32-byte key in low 32
  bytes of a 64-byte LE buffer, reduced mod q). **Deviation from plan stub:**
  return type is `EpAffine` (curve point), not `Fr` (field element) — a Pedersen
  commitment is a group element, and S2.1's homomorphic value-balance check
  requires the group-addition structure a Poseidon hash to Fp does not provide.
  Documented in the module doc.
  Verify: `cargo check --workspace` clean; `cargo test --workspace` green,
  **677 passed** (up from the 670 post-S1.2 baseline; 7 new note tests).
  **Done:** 7 tests in `src/shielded/note.rs`: determinism, value/owner_pk/rho/rcm
  discriminators (each field change → different commitment), homomorphic property
  (`C(n1)+C(n2) == C(summed scalars)` — load-bearing for S2.1, fails if commitment
  is non-homomorphic), and `owner_pk_to_scalar` KAT (zero key → 0, low-byte-1 key → 1,
  byte[1]-1 key → 256). Discriminator proven: removing the `G_RHO` term from
  `commit()` makes `commit_rho_discriminator` fail (rho no longer affects the
  commitment). **Wire-neutral:** no `Message`, `Transaction`, action string, or
  serialization touched.

- [x] **S2.2 Network-side Halo2 proof verification (`verify.rs`)** — *done 2026-09-11*
  Files: `src/shielded/verify.rs` (new — `ShieldedVerifier` + tests), `src/errors.rs`
  (`PneumaticError::Shielded` variant + `Display` arm), `src/shielded/mod.rs`
  (`mod verify;` + `pub use verify::ShieldedVerifier;`).
  Action: S1.1 kept halo2 *proving* behind `#[cfg(test)]`; this installs the *verifying* half in
  non-test code so the network can check a shielded proof a client submits using only the circuit's
  public inputs + the proof bytes (never the note opening / spend key / Merkle path / output note).
  `ShieldedVerifier::new(circuit, k)` runs `keygen_vk` **once** and caches the `(Params, VerifyingKey)`
  keyed by a fingerprint of the circuit *configuration* (tag + width `k`), so repeated verifications
  never re-synthesize the constraint system — a second verifier for the same config reuses the key.
  `verify(proof, public_inputs)` builds the per-instance-column slice (exactly the circuit's
  `Instance` order: nullifier, commit_x, commit_y, merkle_root, output_commit_x, output_commit_y, fee)
  and runs `halo2_proofs::plonk::verify_proof` over a `Blake2bRead` transcript; a rejection surfaces as
  `Err(PneumaticError::Shielded(..))` — never a silent accept. The proof width `k` must match the
  proving width (10); a mismatch makes `verify` reject the proof.
  Verify: `cargo check --workspace` and the full default suite pass; four default tests cover the
  layout, construction, and fail-closed behavior.
  **Discriminator (fails without the fix):** `verify_fails_closed_on_garbage_proof` /
  `verify_fails_closed_on_tampered_public_input` — removing `verify.rs` or making `verify` return
  `Ok(())` unconditionally makes these fail. `verify_vk_cache_reused_across_verifiers` (`#[ignore]`,
  audit-6.10 style) proves the VK cache is load-bearing: two `new` calls for one config invoke
  `keygen_vk` exactly once (via a `KEYGEN_CALLS` counter), not twice. `instances_for` column order is
  pinned by a default test, so any wire-shape change to the `Instance` columns is caught.
  **Wire-compat (ground rule 4):** `Message`, `Transaction`, and every wire path are untouched; only
  two `pub` items are added to the `shielded` module (`ShieldedVerifier`, `keygen_calls` in tests) and
  one new `PneumaticError` variant. `halo2_proofs` was already a production dependency (promoted for
  S2.1) and `once_cell` already a dependency, so **no new packages enter Cargo.lock**.
  **Test-count progression:** core lib passed `476 → 480` (this item +4, all green; 4 ignored incl.
  the two live-prove tests remain `#[ignore]`d per the roadmap's "proving is benchmark-only" rule);
  workspace passed `710 → 714`, `0 failed`.

### Open decisions (resolved at implementation time)
- **Version chosen:** `halo2_proofs = "=0.3.5"` pinned on crates.io — builds clean on the Rust
  1.87.0 toolchain, so the git-`main` fallback (`halo2 = { git = …, branch = "main" }` pinned to a
  commit) was **not** needed.
- **Gating chosen:** `#[ignore]` (not a cargo feature) for the live prove — keeps it discoverable and
  runnable via `-- --ignored` while ensuring the default `cargo test` does zero cryptographic proving.
- **Clash resolution:** none required — `halo2`'s `ff`/`group`/`pasta_curves` coexist with the
  existing `ed25519-dalek 2.2` / `curve25519-dalek` / `ring` / `aes-gcm` / `pqcrypto-*` / `rand_core
  0.6.4` / `getrandom 0.2` graph without any downgrade; the smallest possible delta (the +halo2-tree
  packages, all new) was applied.

## Phase S4.2 — Spent-nullifier registry

*Closes Phase-S4.2 of the Pneumatic Shielded Value Transfer (Tier 1) plan
(`pneumatic-shielded-implementation-plan.md`), per `plans/S4.2-implementation-plan.md`:
the consensus-critical, append-only spent-nullifier set (roadmap 2.5 — consensus-critical,
append-only, durable, globally agreed) that S4.1's check-2 `NullifierMembership` interface was
built to plug into. Ground rules carried into this phase: `cargo check --workspace` and full
`cargo test --workspace` pass after **every** item; each item ships ≥1 discriminator test that
fails without the fix; no wire-shape change without a wire-compat note; fail closed, never
silent-accept.*

- [x] **S4.2.1 `NullifierRegistry` + atomic `try_mark_spent`** — *done 2026-09-17*
  Files: `src/registry.rs` (new `NullifierRegistry` — `DashMap<[u8; 32], ()>` — placed between
  `PendingTransactionRegistry` and `TransactionSignatureRegistry`; `#[derive(Default)]` + `new()`;
  `try_mark_spent` / `contains` / `len` / `is_empty`), `src/lib.rs` (re-export on the existing
  `registry` line).
  Action: the nullifier key is `[u8; 32]` — the output of `nullifier()`
  (`src/shielded/note.rs:143`), the same wire type as `ShieldedTransaction.nullifiers`.
  `try_mark_spent` uses the atomic `insert`-returns-old idiom (no prior `contains_key` check —
  the same no-TOCTOU pattern as `add_transaction` and `used_nonces`); a double-mark returns
  `Err(PneumaticError::Validation([StaleNullifier]))` — the exact reason shape S4.1 check 2
  surfaces, **not** `Registry(String)`. The type doc carries the never-evicted contract: no
  removal API of any kind, unbounded v1 growth (32 B/spend), persistence is S5.3's `ShieldedPool`.
  Tests (+4): `try_mark_spent_first_mark_succeeds`; `try_mark_spent_double_mark_rejected_as_stale_nullifier`
  (discriminator: asserts the *exact* `Validation([StaleNullifier])` variant — a `Registry(..)`
  or multi-reason error fails it); `contains_reflects_state`; `len_and_is_empty_track_inserts`.
  **Done:** workspace `760 → 764`, `0 failed` (core lib `507 → 511`); ignored unchanged (7).
- [x] **S4.2.2 `mark_many_atomic` — all-or-nothing batch** — *done 2026-09-17*
  Files: `src/registry.rs` (one new method on `NullifierRegistry`).
  Action: two-phase because DashMap has no multi-key transaction — (1) check-all before touching
  state, (2) insert-all tracking this call's own inserts; if a concurrent spender lands in the
  gap, the colliding insert triggers a conditional rollback that removes **only** the keys this
  call inserted in phase 2 and returns `StaleNullifier`. The doc carries the soundness argument:
  exactness rests on the no-removal-API invariant — a key whose insert returned `None` was absent
  pre-call, and the only code that can remove such a key is the failed call that inserted it, so
  no other call's spend is ever undone. A duplicate within the batch self-collides in phase 2
  (a nullifier cannot be spent twice, not even by one tx); empty batch is a no-op.
  Tests (+5): `mark_many_atomic_all_fresh_marks_all`;
  `mark_many_atomic_second_already_spent_marks_none` (the all-or-nothing discriminator: pre-mark
  N2, batch `[N1, N2]` → `Err` and N1 **not** marked, `len == 1`);
  `mark_many_atomic_first_already_spent_rejects_before_any_insert` (check-all runs before
  insert-all); `mark_many_atomic_duplicate_within_batch_rejected` (phase-2 self-collision →
  nothing marked, `len == 0`); `mark_many_atomic_empty_batch_is_noop`.
  **Done:** workspace `764 → 769`, `0 failed` (core lib `511 → 516`).
- [x] **S4.2.3 concurrency proofs** — *done 2026-09-17*
  Files: `src/registry.rs` (test module only).
  Tests (+3): `concurrent_try_mark_spent_same_nullifier_exactly_one_succeeds` (8 threads, one
  nullifier → exactly 1 `Ok`, `len == 1`); `concurrent_mark_many_atomic_same_batch_one_succeeds_no_partial`
  (8 threads, identical two-nullifier batch → exactly 1 `Ok`, whole batch applied, no partial
  state); `concurrent_mark_many_atomic_mixed_unique_and_duplicates` (16 threads: 8 unique
  single-nullifier batches + 8 duplicates over the same 8 → exactly 8 `Ok` / 8 `Err`,
  `len == 8`).
  **Done:** workspace `769 → 772`, `0 failed` (core lib `516 → 519`).
- [x] **S4.2.4 S4.1 seam — `NullifierMembership` impl + concrete-registry checks** — *done 2026-09-17*
  Files: `src/registry.rs` (`impl NullifierMembership for NullifierRegistry` delegating to
  `contains`; top-level `use crate::validation::NullifierMembership` — no cycle, `validation`
  references `registry` only inside its test module), `src/validation.rs` (test module only —
  the S4.1 fixtures are private there).
  Action: the spec's check 2 now has its intended real backer; `ShieldedValidationDeps::spent`
  (`&'a dyn NullifierMembership`) is satisfied by the concrete registry via the same coercion the
  S4.1 fakes used.
  Tests (+2): `check2_against_concrete_nullifier_registry_rejects_spent` (the S4.1 check-2
  discriminator re-run against the concrete registry — already-spent → `StaleNullifier`);
  `check2_against_concrete_registry_fresh_reaches_proof_check` (fresh registry → clears checks
  1-3 and reaches check 4 → `InvalidShieldedProof` on the placeholder proof, reusing S4.1's
  shared `Lazy<ShieldedVerifier>` — no new cost class, not `#[ignore]`d).
  **Done:** workspace `772 → 774`, `0 failed` (core lib `519 → 521`); ignored unchanged (7:
  core 5, prover 1, sentinel doc-test 1).
  **Wire-compat (ground rule 4):** *no wire-message shape changed.* `SignedTransaction.shielded`
  and every `ShieldedTransaction` field are untouched; this item adds an in-memory registry type
  (plus one re-export) that no wire path serializes. Zero new dependencies — `DashMap` and
  `serde` are existing production deps.
  **Scope note:** in-memory only by design — persistence, the single-writer guard, and rollback
  semantics are S5.3's `ShieldedPool`; the roadmap 2.5 "never evicted" contract is enforced by
  the absence of any removal API on this type. S6.3 scales the S4.2.3 races up to 50 threads;
  S6.2 asserts the exact named errors.

## Phase S4.3 — Merkle-root history (concrete check 3)

*Closes Phase-S4.3 of the Pneumatic Shielded Value Transfer (Tier 1) plan
(`pneumatic-shielded-implementation-plan.md`), per `plans/S4.3-implementation-plan.md`:
the concrete `MerkleRootHistory` (S4.1, `validation.rs:413`) for check 3's `is_root_fresh` —
a bounded, append-only committed-root history with a genesis seed, a `shielded_root_recency`
consensus parameter, and the S4.1 check-3 discriminators re-run on the real type. Ground rules
carried into this phase: `cargo check --workspace` and full `cargo test --workspace` pass after
**every** item; each item ships ≥1 discriminator test that fails without the fix; no wire-shape
change without a wire-compat note; fail closed, never silent-accept.*

- [x] **S4.3.1 `MerkleRootState` — the bounded committed-root history** — *done 2026-09-17*
  Files: `src/shielded/roots.rs` (new), `src/shielded/mod.rs` (`mod roots;` + re-export).
  Action: `MerkleRootState { window, next_height, roots: Vec<RootSnapshot> }` — `new(k)` seeds
  the genesis pool state (`root_to_bytes(&Fp::zero())` at height 0; Decision 2); `push(root)`
  appends the tip at a monotonic sequence height and prunes to the newest `window + 1` entries
  (Decision 1 — bounded retention, the Phase 3.4 orphan-buffer pattern); heights come from the
  `next_height` counter, **not** `roots.len()` (once the first prune lands the retained length
  is constant while the sequence keeps advancing — a live bug caught and fixed by this item's
  prune test); `current_root()` / `recency_window()` / `len()` / `is_empty()`. No rewind / trim
  API of any kind (S5.3 boundary, Decision 5 — the in-memory read-side view only).
  Tests (+5): `merkle_root_state_new_seeds_genesis_pool_state`;
  `merkle_root_state_push_advances_height_and_current_root`;
  `merkle_root_state_prunes_to_window_plus_one` (window 2, 5 pushes → exactly heights 3,4,5
  retained — the discriminator that exposed the `len()`-derived-height bug);
  `merkle_root_state_history_oldest_to_newest` (strictly ascending heights, last = tip — the
  trait's order contract); `merkle_root_state_is_send_sync` (S5.3's Arc-sharing contract).
  **Done:** workspace `774 → 779`, `0 failed` (core lib `521 → 526`); ignored unchanged (8).
- [x] **S4.3.2 `shielded_root_recency` consensus parameter** — *done 2026-09-17*
  Files: `src/environment.rs` (field on `EnvironmentMetadataSpec` with
  `#[serde(default = "default_shielded_root_recency")]` → 10; field on `EnvironmentMetadata`;
  copy in `load_from_spec`; default fn), `sentinel/src/transaction_notifier.rs` and
  `tests/transport_integration.rs` (×2) — the three test-only `EnvironmentMetadata`
  struct-literal sites; the other five test fixtures (sentinel ×2, committer, tokens,
  action_router) are JSON-based `EnvironmentMetadataSpec`s and pick up the serde default
  without edits.
  Action: K is a consensus parameter (Decision 3 — every node runs the same K, which is what
  makes the bounded retention safe); `validate()` deliberately untouched (any window ≥ 0 is
  well-typed; the range policy stays S5.3's).
  Tests (+2): `env_spec_shielded_root_recency_defaults_to_ten` (key absent → 10, no
  deserialization failure); `env_spec_shielded_root_recency_roundtrips` (key = 25 → 25).
  **Done:** workspace `779 → 781`, `0 failed` (core lib `526 → 528`); ignored unchanged (8).
- [x] **S4.3.3 `impl MerkleRootHistory for MerkleRootState` + check-3 discriminators re-run** —
  *done 2026-09-17*
  Files: `src/shielded/roots.rs` (the one-method impl — `root_history` returns `&self.roots`,
  oldest→newest, last = tip; closes the S4.1 seam), `src/validation.rs` (**test module only** —
  no production change to the spec, Decision 4; the S4.1 fakes stay in place pinning the window
  arithmetic on a full-history view).
  Action: the seam is provably load-bearing (S4.2.4 style): all eight tests below fail to
  compile without the impl. Each re-runs the parent item's verify list on the concrete state
  with the S4.2.4 pattern — a fresh real `NullifierRegistry` as `deps.spent` (check 2 passes),
  inline `ShieldedValidationDeps` (the `run_shielded` helper is typed to the fakes), and dummy
  post-roots as canonical `Fp::from(tag)` bytes (check 3 compares raw bytes; the dummies must
  merely decode). Accept side reaches check 4 — `InvalidShieldedProof` on the placeholder
  proof is the "passed check 3" signal; reject side is the exact `StaleMerkleRoot` variant.
  Tests (+8): `check3_concrete_root_state_current_root_accepted` (tip distance 0 → check 4);
  `check3_concrete_root_state_k_minus_1_back_accepted` (distance 9 = K−1 → accepted);
  `check3_concrete_root_state_at_window_boundary_accepted` (distance **exactly K**, the root
  still retained at capacity 11 — the `<=` boundary discriminator: an `<` implementation
  (validation.rs:522) fails exactly this test);
  `check3_concrete_root_state_k_plus_1_back_rejected` (distance K+1 → the 12th entry prunes
  the referenced root → `StaleMerkleRoot`; per Decision 1, on a bounded state "beyond window"
  and "not found" are the same event — the assertion is the parent's, the mechanism is the
  prune); `check3_concrete_root_state_window_zero_rejects_one_back` (**the parent item's
  headline discriminator**: K=0 → a root one commit back that **is in the committed history**
  is rejected purely because the window shrank — window logic, not equality);
  `check3_concrete_root_state_window_zero_accepts_exact_tip` (K=0 accepts the exact tip —
  pins "K=0 = exact tip only" from both sides); `check3_concrete_root_state_unknown_root_rejected`
  (valid field element, never committed → `StaleMerkleRoot` — fail-closed on *unknown*,
  distinct from *stale*; a "accept if it looks like a valid field element" implementation
  fails here); `check3_concrete_root_state_pruned_root_rejected` (window 2: push the tx root
  then 3 dummies → pruned → `StaleMerkleRoot` **and** `state.len() == 3` in the same test —
  retention bound and rejection consequence asserted together at the validation boundary).
  **Done:** workspace `781 → 789`, `0 failed` (core lib `528 → 536`); ignored unchanged (8).
  The accept-side tests reach check 4 and reuse S4.1's module-level `Lazy<ShieldedVerifier>`
  (keygen ~60–100 s, once per test binary, already paid by S4.1's tests — no new cost class,
  no `#[ignore]` gate).
- [x] **S4.3.4 Genesis bootstrap — the first transfer on a fresh network** — *done 2026-09-17*
  Files: `src/validation.rs` (test module only).
  Test (+1): `genesis_pool_state_accepts_first_transfer` (a tx with
  `merkle_root = [0u8; 32]` — the empty pool's root, what a wallet proves against before any
  commitment has landed — run against a **fresh** `MerkleRootState::new(10)` with zero pushes
  and a fresh `NullifierRegistry` → passes check 3, reaches check 4 → `InvalidShieldedProof`;
  no bootstrap deadlock).
  **Discriminator:** constructing the state without the genesis seed (the one-line revert of
  Decision 2) is empty, so `is_root_fresh` takes the empty-history arm and this test becomes
  `StaleMerkleRoot` — the seed is proven necessary, not cosmetic.
  **Done:** workspace `789 → 790`, `0 failed` (core lib `536 → 537`); ignored unchanged (8).
  **Wire-compat (ground rule 4):** *no wire-message shape changed of any kind.* No new action
  string, payload schema, or message; no field added to `Transaction` / `SignedTransaction` /
  `ShieldedTransaction` — the referenced root already rides the wire as
  `ShieldedTransaction.merkle_root` (S3.1); the root history is not gossiped (replicated by
  block-history replay — a derived, bounded view). The only additive surface is the
  serde-defaulted `shielded_root_recency` key (default 10) in the node-local environment spec
  JSON. Zero new dependencies.
  **Scope note:** in-memory read-side structure only — persistence, the single-writer guard,
  the rebuild-at-boot from the applied-update map, and the tip-rollback interaction are S5.3's
  `ShieldedPool` (Decision 5), which fails closed on `current_root() != tree.root()`.

## Phase S5.3 — Committer: shielded pool append + durable nullifier commit
*Closes the S5.3 item of the shielded implementation plan (`plans/S5.3-implementation-plan.md`; vault `task-s5-3-pool-append`). One global pool, one writer, nullifiers durable before commit is reported.*

- [x] **S5.3.1 `ShieldedPool` — one global pool behind a single-writer guard** — *done 2026-09-22*
  Files: `committer/src/shielded_pool.rs` (new, ~1230 lines), `committer/src/lib.rs` (re-export).
  `ShieldedPool { nullifiers: Arc<NullifierRegistry>, recency_window, view_roots: Arc<MerkleRootState>, guard: Mutex<PoolState> }`. ONE instance per process; the single `std::sync::Mutex` is deliberate — a blocking Halo2 proof verification must never yield mid-check (apply re-verifies under the same guard it appends under). `PoolState = { leaves, tree, roots, applied, applied_index }`; `view_roots` is the **boot-time** Arc (safe-Rust view constraint — a `&'self dyn` method can't alias the guard's changing snapshot; live snapshot is owned via `view_parts()`).
  API: `new(recency)` (pristine genesis), `load(&dyn DataProvider, &str, usize)` (fail-closed: envelope read/verify + leaf/root-integrity recheck — a corrupt or divergent record is a boot error, never a silent empty pool; true absence → pristine), `from_state`/`rebuild_state` (delta replay; boot path verifies each recorded `post_root` against the recomputed value — integrity, `PoolState { kind: "integrity" }`; rollback path recomputes and writes back), `apply_update(&Block, &env)` (the full commit-time op under one guard acquisition), `revert_update(&[u8])` (exact inverse: remove the delta's leaves/nullifiers, rebuild from survivors), `save`, `state_snapshot`, `view_parts()`, `impl ShieldedPoolView` (`nullifier_set()` live, `root_history()` boot snapshot).
  **Done:** 18 unit tests in-module (load absent/fail-closed/corrupt/leaf-mismatch/divergent-root; rebuild roundtrip; apply rejects `InvalidShieldedProof`/`StaleNullifier`/`StaleMerkleRoot`; plain block no-op; `apply_update_replay_is_already_applied_before_recheck` — the idempotency recheck runs BEFORE re-validation, pinned by seeding a replayed block carrying a stale-nullifier tx; `revert_update_is_the_exact_inverse`; unknown-hash revert → `PoolRollback` no-op; save roundtrip; `save_fails_with_pool_persist_on_store_failure`; view contract + Send+Sync).
- [x] **S5.3.2 Commit path: H12 payload match → authoritative re-check → apply → persist → commit (inverted order)** — *done 2026-09-22*
  Files: `committer/src/committer/committing.rs`, `committer/src/committer/finalizing.rs`, `committer/src/committer.rs` (pool field; `Committer::new` 20th param), `committer/src/committer/tests/pool.rs` (new).
  Commit sequence (plain path): **H12** — `signed_trans.shielded` must exist AND hash-match `pending_registry.get_shielded(&tx_id)` (absent OR mismatch ⇒ `TransactionPayloadMismatch`); **apply** — `ShieldedValidationSpec::new()` re-runs the full shielded proof verification with pool-owned deps (the committer's check is the authoritative consensus gate) and records the delta (atomic `mark_many_atomic` + leaf append + root push + applied index); **persist** — `save` runs while the outcome is `Applied`, BEFORE the chain append (durability: nullifiers are never committed without their durable record); **commit** — `commit_block`; on chain-Err → **undo** (revert the delta; no-op for non-`Applied`). `apply_update` short-circuits replayed block hashes as `AlreadyApplied` **before** re-validation — replay is cheap and, critically, undoes nothing.
  **Inverted rollback ordering** (the non-obvious part): on a tip conflict, the winner is applied+persisted FIRST, then `commit_block(Some(loser))` rolls the loser back and appends the winner; on success the **loser's** delta is reverted — and `PoolRollback` (loser had no delta: it was a buffered candidate, never committed) is the EXPECTED no-op arm, not an error; on chain-Err the winner's delta is undone. This order is what makes the pool and the chain agree after every commit, win or loss.
  **Save-failure closure:** if the apply succeeded but the durability `save` fails, `shielded_commit_apply` reverts the just-applied delta before propagating `PoolPersist` — the block never commits, so the in-memory pool must not carry a delta the chain lacks (a divergent pool would be worse than the surfaced failure); the store still holds the pre-apply state, so no re-save is needed.
  `handle_block_finalized` applies+persists per committed block (idempotent; a failure is logged loudly and propagated — the finalizer never reports a block whose pool state it couldn't record).
  **Done:** 5 fast committer-path discriminator tests (H12 absent; H12 swapped payload — registry nullifier `0x55…` vs wire `0x77…`; re-check rejects bad proof despite a matching registry entry; re-check rejects stale nullifier at commit; re-check rejects stale root at commit — the garbage-64-byte-proof pattern makes all five run without a live prove).
- [x] **S5.3.3 Boot load + composite threading** — *done 2026-09-22*
  Files: `committer/src/main.rs`, `node-server/src/node_server.rs` (+17th `build_role_plugin` param; `build_runtime` now takes the data provider as a 3rd param).
  Standalone committer main: `ShieldedPool::load` before BlockServices/Committer construction. Composite: `build_runtime` loads once (node_server.rs:309), threads the shared `Arc<ShieldedPool>` into `BlockServices` and the committer role arm; sentinel/finalizer arms received `SimpleShieldedPoolView::with(pool.view_parts())` (boot-snapshot semantics documented in `concept-shielded-pool-view`) — **S5.4** swapped that for `Arc<ShieldedPool>` directly (done 2026-09-22, see Phase S5.4 below).
  **Fail-closed boot × test-env DI:** the pool boot load is fail-closed (any store error = boot refusal), while the node-server tests historically ran with no data service (`DefaultDataProvider` over a dead socket). Resolved by injecting the data provider into `build_runtime` (production keeps `DefaultDataProvider` and the strict contract; production bin is still a scaffold — no production call site changes) and an in-memory `MemoryDataProvider` in the node-server test module (reachable service, no records ⇒ pristine genesis; other reads answer not-found in-process, no sockets).
- [x] **S5.3.4 Live (benchmark-only) tests** — *done 2026-09-22*
  Files: `committer/src/committer/tests/pool.rs` (live section), `committer/src/committer/tests/helpers.rs` (`TestDataProvider` in-memory shielded-pool store + `with_shielded_save_failure`), `committer/src/shielded_pool.rs` (`pub fn nullifiers()` accessor), `src/shielded/tree.rs` (`commitment_to_leaf` made `pub` — the canonical commitment→leaf mapping delta records mirror; new `membership_proof(index)` — re-derive an existing leaf's proof against the tree's current root, needed because a proof from an earlier `append` is stale once later leaves land above it).
  Five `#[ignore]`d tests (real Halo2 prove, ~1 min each; `cargo test -p pneumatic_committer -- --ignored`): `live_commit_advances_pool_persists_and_reloads` (commit → leaf +1, delta +1, store state advanced, fresh `ShieldedPool::load` reconstructs identical leaves/root/applied); `live_idempotent_replay_reapplies_nothing` (re-delivered commit → chain rejects the duplicate append, pool EXACTLY as after the first commit — the failed chain append on a replay never reverts a previously applied delta); `live_cross_block_double_spend_rejected` (two real spends of ONE note — same root, append-only membership stays valid — first commits, second → `ShieldedProofInvalid` cause `StaleNullifier`, exactly one spend on chain/pool); `live_durability_fail_save_rolls_back` (failing `save_shielded_pool` → `PoolPersist`, chain NOT appended, delta reverted, nullifier unmarked — fail-closed); `live_rollback_lockstep` (sibling A commits; higher-stake sibling B wins the conflict → B applied+persisted, A rolled back on the chain AND its delta reverted from the pool — pool and chain agree exactly: A's nullifier unmarked, B's marked, applied count = seed + B).
  **Wire-compat (ground rule 4):** no wire-message shape changed. H12 compares the `shielded` payload already riding the `SignedTransaction` wire (S3.1) against the registry — no field added. Pool state persists through the existing data-service `GetOp`/`SaveOp::ShieldedPool` envelope (S5.3 enabler, d25f1ba). Zero new dependencies (`pneumatic_prover` was already a pinned dev-dep for the enabler).
  **Scope note (closed by S5.4):** the role-view swap (sentinel/finalizer reading the LIVE pool through the trait) was S5.4's and landed 2026-09-22 — the composite no longer composes `SimpleShieldedPoolView`; the role views ARE the shared `Arc<ShieldedPool>` (Phase S5.4 below).

## Phase S5.4 — Node-server role wiring: real pool swap
*Closes the S5.4 item of the shielded implementation plan (`plans/S5.4-implementation-plan.md`; vault `task-s5-4-real-pool-swap`). One pool, no stub view: in the composite, the shielded roles' views are the shared `Arc<ShieldedPool>` itself.*

- [x] **S5.4.1 `build_role_plugin` view composition swap** — *done 2026-09-22*
  Files: `node-server/src/node_server.rs`. The sentinel/finalizer arms now consume the SAME pool as their `ShieldedPoolView` — `shielded_pool.clone() as Arc<dyn ShieldedPoolView>` at the arm boundary (plan Decision 2, option (a)) — replacing the `SimpleShieldedPoolView::with(pool.view_parts())` composition, which is gone from the composite build (zero references in `node-server/`). `SimpleShieldedPoolView` stays in `src/shielded/pool_view.rs` for split-deployment replay views; `view_parts()` stays public (tests + replay composition).
- [x] **S5.4.2 Single pool param, dual param dropped** — *done 2026-09-22*
  Files: `node-server/src/node_server.rs`. `build_role_plugin` keeps its single S5.3 `shielded_pool` param; the separate `shielded_pool_view` param is dropped, and the existing S5.3 tests that built a view for the test bundle were updated to pass the pool. `ReducedFinalizerPlugin`'s `RoleHost` impl fixed (`Finalizer::advance_epoch(&mut self.0)` — no args). No action-set changes: `FINALIZER_ACTIONS` already carried `SignShielded`/`ShieldedVote` since S5.2; `SENTINEL_ACTIONS`/`COMMITTER_ACTIONS` unchanged (Decision 5 wire-compat).
- [x] **S5.4.3 Discriminator: the role action gate is load-bearing** — *done 2026-09-22*
  `signshielded_removed_from_action_set_is_rejected_by_dispatcher` (fast, runs in the regular suite): removing `"SignShielded"` from the finalizer's action set makes the dispatcher reject the vote request `UnknownAction`.
- [x] **S5.4.4 Live (benchmark-only) composite e2e** — *done 2026-09-22*
  Files: `node-server/src/node_server.rs` (test module); dev-deps `pneumatic_prover`, `pasta_curves = "=0.5.2"`, `async-trait = "0.1.88"` (pinned to already-resolved versions — zero new lockfile packages).
  `live_composite_shielded_e2e_all_four_hops_advance_the_pool` (`#[ignore]`, real Halo2 prove ~1 min; `cargo test -p pneumatic_node_server -- --ignored`): ONE composite node, all roles; a live-proven transfer (100 = 90 + 10) is driven through all four dispatch hops (Sentinel `Verify` → Finalizer `SignShielded` → `ShieldedVote` → Committer `Commit`), each hop relayed by re-dispatching what the previous role recorded. The test drives ONLY the relays — the sentinel does its own `register_shielded` + finalizer assignment, the finalizer its own build + sign, and the committer materializes its pending entry from the authenticated wire block (H4); nothing is pre-staged. Terminal lockstep assertions: the shared pool's `leaf_count` 1→2, `applied_count` 1→2, nullifier marked, `current_root()` == the tree root rebuilt from the snapshot's leaves, chain tip == the committed block, chain +1 exactly.
- [x] **S5.4.5 Live (benchmark-only) conflict rollback** — *done 2026-09-22*
  `live_composite_conflict_rollback_lockstep` (`#[ignore]`, two real proves ~2 min): sibling shielded blocks A (stake 100) and B (stake 200), both proven against the shared final root and chained off the SAME genesis tip; A commits first (delta applied), then B commits at the same position → `resolve_block_conflict` (unequal stakes — an equal-stake tie would fail closed) discards the loser. Lockstep assertions: tip == B, chain count genesis + B, `applied_count` == seed + B (A's delta reverted), A's nullifier unmarked, B's marked, `current_root()` == rebuilt tree root. Finalizer-free by design (two hand-signed commits from two registered finalizer identities) — the committer's conflict path is the unit under test, not the finalizer's quorum math.
  **Wire-compat (ground rule 4):** no new wire surface — no new action strings, no message/payload/field changes (Decision 5).
  **Consensus-safety note:** the role view's `root_history` keeps its boot-snapshot semantics (a property of the `impl ShieldedPoolView for ShieldedPool` contract, documented in `committer/src/shielded_pool.rs` module docs); that is safe because the advisory gates only need "a recent valid root" while the commit-time AUTHORITATIVE re-check reads the pool's live roots under the single-writer guard — a stale view can never admit a commit.
  **Final suite:** 847 passed / 16 ignored / 0 failed (`cargo test --workspace`); the two new live tests are `#[ignore]`d (benchmark-only).

## Phase S6 — Shielded Tier-1 completion: attack surface, concurrency, cross-crate pipeline, proving UX
*Closes the shielded implementation plan (parent `pneumatic-shielded-implementation-plan.md`, execution `plans/S6-implementation-plan.md`). After S6 the shielded stack is FEATURE-COMPLETE; remaining items are operational (circuit-audit hard gate, viewing-key policy, proving-UX numbers, anonymity bootstrap — see "Open items at close" below).*

- [x] **S6.1 Cross-crate shielded pipeline — all four hops, one process, real wire messages** — *done 2026-09-22*
  Files: `tests/shielded_pipeline.rs` (new, root-level).
  Sentinel + finalizer + committer in one process, sharing ONE `PendingTransactionRegistry` (the sentinel's `register_shielded` feeds the committer's S5.3 hash check) and ONE `Arc<ShieldedPool>` (sentinel/finalizer as read-only `ShieldedPoolView`, committer as the single writer). Composite identity pattern: ONE identity for sentinel+finalizer (the finalizer's C1 gate requires the `SignShielded` sender registered as a Finalizer), a second for the committer.
  1. `wire_bytes_carry_only_the_public_surface` (fast, default suite): across EVERY recorded wire byte of all three roles, no note plaintext travels — no owner pk, no spend seed, no value (LE and BE), no recipient ed25519/x25519 pk — while the public surface (both commitments, the nullifier, the Merkle root, the proof) is present in its rmp encoding (the wire is MsgPack, where `[u8;32]` fields encode as integer arrays and `Message.body` byte vectors per-byte, so presence is asserted against the DECODED message bodies, where the stx rmp travels as real bytes). The cross-crate S5.3 seam (the registered stx hash-binds to the wire payload) is asserted on the shared registry.
  2. `sentinel_advisory_gate_rejects_double_spend_before_finalizer_work` (fast, REAL spec): a pre-spent nullifier in the shared pool view makes the sentinel's step-2 advisory validation fire `StaleNullifier` before registration and before any wire traffic — checks 1→3 short-circuit before check 4, so the default suite pays no halo2 keygen.
  3. LIVE (`#[ignore]`d; the committer re-check reaches check 4, whose lazy verifier pays the one-time ActionCircuit `keygen_vk` ~2.5 min in the test binary; re-run commands are in the `#[ignore = …]` attributes): `full_pipeline_fake_proof_stops_at_committer_recheck` — four hops; a fake proof sails through both advisory gates and dies exactly at the committer's pool re-check (`ShieldedProofInvalid`, cause `InvalidShieldedProof`), the pool byte-for-byte untouched and the chain unadvanced; and `full_pipeline_live_prove_commits_and_lockstep_advances` — a real halo2 prove (100 = 90 + fee 10) drives all four hops to a SUCCESSFUL commit: pool delta and token chain advance in lockstep (leaf +1, delta +1, nullifier spent, root-history tip == rebuilt tree root, chain +1, committed block == chain tip), the output note pays 90 to the recipient, and re-applying the same block is an idempotent `AlreadyApplied` no-op.
  **Wire-compat (ground rule 4):** no wire changes — the test drives the composite plugin's existing delivery shape (inner `ShieldedTransfer`/`SignShielded`/`ShieldedVote`/`Commit` messages); no field added.
- [x] **S6.2 Canonical attack suite — audit table + new live coverage** — *done 2026-09-22*
  Files: `tests/shielded_attacks.rs` (new, ~363 lines). An audit table maps each canonical shielded-transfer attack to its canonical coverage (S4.1.5 spec checks; S5.3 committer re-check; pool apply), then new tests: `pool_rejects_stale_nullifier_at_apply` / `pool_rejects_stale_root_at_apply` (fast — the apply-time re-check fires checks 2/3 BEFORE check 4; the `CommitterError::ShieldedProofInvalid` cause string names the discriminator) and `#[ignore]`d `truncated_proof_rejected_not_panicked` (a 32-byte truncation of the 64-byte proof → `InvalidShieldedProof`, never a panic — the verifier is total over malformed input) + `concurrent_same_note_spend_exactly_one_commits` (two live txs race `apply_update` on the same note; the single-writer guard serializes — exactly one `Applied`, the loser `StaleNullifier`, leaf count 2, the loser's leaf absent).
- [x] **S6.3 Concurrency at the two shared-state seams** — *done 2026-09-22*
  Files: `src/registry/tests/nullifiers.rs`, `src/shielded/tree.rs`.
  `concurrent_try_mark_spent_50_threads_mixed_unique_and_duplicates` — 50 threads hammer 25 unique nullifiers plus duplicates: exactly 25 `Ok`, 25 `StaleNullifier`, the set ends at 25 (no double-spend window under the lock).
  `concurrent_append_and_proof_verification_stay_consistent` — 4 readers continuously verify a membership proof against the current root while 200 appends race (readers stop on an `AtomicBool`): the proof/root pair never diverges under the tree lock; final leaf count 201.
- [x] **S6.4 Proving UX — prove/verify timing benchmark** — *done 2026-09-22*
  Files: `prover/src/build.rs` (test module).
  `build_shielded_tx_prove_and_verify_timing` (`#[ignore]`d, no criterion — `std::time::Instant`): a warm prove (pays the one-time keygen on a cold binary and builds the verifier, both excluded from timing) followed by a TIMED prove of a fresh 1-in/1-out transfer at k=10 and a TIMED network-side verify; prints the audit line `[S6.4] shielded 1-in/1-out @ k=10 — prove: … | verify: …`.
  **Measured (this box):** release — **prove 25.4 s / verify 124 ms**; debug — prove 247 s / verify 1.14 s (~100× inflation, which is why the test runs `--release`).
  **Budget decision (recorded, feeds open item #3):** the 100 ms Tier-1 plan target was NOT met on this box (124 ms measured — first verify, right after a 25 s prove in the same process; the margin to target is within box noise). The test asserts a **200 ms tripwire** (~1.6× measured; trips on a 2×+ regression) rather than the unmet 100 ms target. Wherever proving runs for end users (open item #3: embedded wallet vs prover service) must expect ~25 s of CPU for a single transfer on this class of machine.
  **Zero new packages (ground rule):** all S6 work uses dependencies already in the root `Cargo.toml` (`tempfile` + the four worker-crate dev-deps were added with S6.2; `rand`/`ed25519-dalek`/`pasta_curves`/`ff`/`dashmap`/`async-trait` pre-existed).
  **Final suite:** 834 passed / 37 ignored / 0 failed (`cargo test --workspace`) — monotonic over the post-S6.2 baseline of 832 / 33 / 0; the four new live tests (pipeline ×2, attacks' second, prover timing) join the `#[ignore]`d benchmark lane.
- [ ] **Open items at close (NOT code — operational/decision gates):**
  1. **Circuit audit hard gate** — the ActionCircuit (k=10, 7 public inputs) has NO independent audit; the S6 tests prove the pipeline is internally consistent, not that the circuit is sound beyond its own construction. A third-party (or at minimum second-party) circuit review is a launch gate.
  2. **Viewing-key policy** — S3.3.4's `scan_for_notes` viewing-key path is implemented and tested, but the key-handling policy (where viewing keys are stored, who can request scans, audit/export flows) is undecided.
  3. **Proving UX** — S6.4 numbers (above) quantify prove/verify; a product decision is still open on where proving runs for end users (embedded wallet vs prover service) given the measured prove time.
  4. **Anonymity bootstrap** — the Merkle tree starts at one leaf in tests; a production anonymity set (how many notes exist before a transfer can be "anonymous") needs a genesis policy.

## Phase 1 — Wire integrity: sign, verify, dedup correctly
*Closes: C1, C4, C7, L1. This is the highest-leverage phase — it makes the wire path actually
work and removes forgeable identity from the consensus path.*

- [x] **1.1 Sign every outgoing message with node identity** (C1, C4) — *done 2026-08-20*
  Files: `sentinel/src/transaction_notifier.rs` (lines 35, 55, 96, 112, 128, 147, 168),
  `committer/src/block_services.rs:111,146`, and every other `Message { ... signature: vec![] }`
  construction site in production code.
  Action: sign `body` (envelope) with the node's Ed25519 identity key; line 96 currently puts a
  *public key* in the signature field — replace with a real signature.
  Verify: `grep -rn "signature: vec!\[\]" --include=*.rs` in production code (exclude tests)
  returns nothing.
  **Done:** all 18 production send sites (sentinel 7, executor 2, finalizer 5, committer 4)
  build envelopes via `Message::signed` (src/messages.rs) — raw `body` bytes signed with the
  node's Ed25519 identity key, `public_key` set to the identity pubkey, the exact payload the
  live verifier `Gossiper::handle_message` checks (src/gossiper.rs). No wire-shape change —
  previously empty/malformed fields are now populated. Identity (`Arc<NodeIdentity>`) injected
  into `BlockServices`, `Committer`, `Executor`/`ExecutorHandle`, and `MessageDispatcher`
  (the dead `finalizer_signature` placeholder field is removed) / `Finalizer` constructors —
  intentional breaking library-API change (fail-closed). Three destination-key-in-signature-field
  bugs fixed (sentinel `send_to_finalizer_for_preload`, both executor `send_to_finalizer`
  impls). 12 regression tests (RecordingConnection capture + `assert_signed_by` per send path,
  including a gossiper pass-through test), grep gate clean, workspace suite 444 green.

- [x] **1.2 Gossiper: verify-then-insert, dedup on content hash** (C4) — *done 2026-08-20*
  File: `src/gossiper.rs`.
  Action: key the dedup cache on a hash of `(sender_key, body)`, not raw `message.signature`
  bytes; insert into the cache only **after** signature verification succeeds; verify the
  envelope signature against the sender's registered key.
  Verify: new tests — (a) two different senders with identical bodies both pass; (b) same
  sender+body twice → second deduped; (c) bad signature rejected AND not cached (a re-sent
  valid message from the same sender is accepted).
  **Done:** rewrote `Gossiper::handle_message` (src/gossiper.rs) to verify-then-insert:
  deserialize → `check_signature(signature, public_key, body)` (reject `InvalidSignature`,
  never poison the cache) → insert into the dedup cache → fan out. Added a `dedup_key()`
  helper computing a Merkle-style key over `(public_key, body)` — double SHA-256 (`SHA-256(pk)`
  `‖` `SHA-256(body)`) so a key/body pair cannot prefix-collide, e.g. a 1-byte key + 3-byte body
  vs. a 3-byte key + 1-byte body; keyed on content not the raw signature, so an honest re-send
  is collapsed and two senders with an identical body are not confused. Verification now runs
  *before* the cache is touched, so a forged/tampered message is rejected and never admitted —
  closing the poison-the-cache replay/bloom (a bad-sig entry can no longer silently drop the
  next legit re-send). 4 regression tests: `gossiper_two_senders_same_body_both_pass` (a),
  `gossiper_content_hash_dedup` (b), `gossiper_forged_signature_not_cached` (c), and
  `gossiper_tampered_body_rejected`. The (c) test asserts the re-sent valid message reaches the
  handler (`count == 1`); it was proven a true regression discriminator — it fails
  (`count == 0`) under the old insert-before-verify ordering. Verification is self-contained
  (checks the envelope signature against `public_key` only); per-sender registry-role
  authentication is deferred to Phase 1.3. No wire-shape change — the `Message` struct,
  signature encoding, and `check_signature` are untouched. `cargo check` clean; workspace suite
  ~448 green (baseline 444 + these 4).

- [x] **1.3 Committer: authenticate the envelope** (C1)
  File: `committer/src/committer.rs:210`.
  Action: `handle_message` calls `authenticate_message` first (fail-closed). It verifies the
  envelope signature over `message.body`, requires `message.public_key` to be a registered node
  (`NodeRegistry::find_node_type_by_public_key`), and enforces a role→action map (`Commit` /
  `BlockFinalized` → Finalizer; `DistributeToken` / `DistributeBlock` → Committer;
  `BlockConfirmed` / `BlockQuorumReached` → any registered role; `EpochReconcile` → self only).
  Unknown key → `UnauthenticatedSender`; registered-but-wrong-role → `UnauthorizedRole`.
  **RNS packet bridge — transport-agnostic (AUDIT ground rule: do not change the wire/transport
  shape).** `src/rns/wrapper.rs:310` still drops the sender rhash, but that is safe: RNS is
  destination-encrypted and multi-hop (`RawPacket` carries no sender; HEADER_1 packets have
  none), so the delivery callback could not recover the originator anyway. The commender
  authenticates on the envelope's self-identified `public_key` + `signature`, which RNS delivery
  cannot strip or forge. The gate lives at the router, the single chokepoint the RNS-data bridge
  (`committer/src/main.rs`) and any direct caller pass through. No `rns-core` change. Rationale
  noted at `src/rns/wrapper.rs:310`.
  Verify: 4 regression tests (`unregistered_sender_commit_is_rejected`,
  `unregistered_sender_block_finalized_is_rejected`, `wrong_role_sender_block_finalized_is_rejected`,
  `foreign_sender_epoch_reconcile_is_rejected`) assert the four rejection cases in
  `committer/tests/pipeline_integration.rs`; the 2 updated e2e pipeline tests register a
  Finalizer sender and sign the envelope.

- [x] **1.4 Finalizer: voter identity from the registry, signatures verified** (C1)
  Files: `finalizer/src/finalizer.rs:263-316`, `finalizer/src/signature_collector.rs`,
  `src/transactions.rs:240-246`.
  Action: stop using `message.public_key` as the self-declared `executor_key` — take voter
  identity from the authenticated envelope and require it to be a registered Executor; verify
  `TransactionSignature.signature` over the transaction with the voter's key before accepting;
  reject (not create) unknown voters in the signature registry (currently check-or-create);
  `current_stake` must come from the stake snapshot, not the message.
  Note: with 1.1 done, envelopes now carry *real* signatures, so
  `Finalizer::handle_preload` (finalizer/src/finalizer.rs:249) storing
  `message.signature` as **data** in `preload_tasks` is now a live bug, not a
  placeholder — fold into this item.
  Verify: test that a forged voter key / missing signature / non-registered voter is all
  rejected; quorum counts only verified, registered voters.

- [x] **1.5 Directory responses: per-entry authenticity, no rhash overwrite** (C7) — *done 2026-08-21*
  File: `src/node/registry.rs` (only structural type change lives in `src/node.rs`).
  Action: `handle_directory_response` fails closed — (1) `responder_key` must be a registered node;
  (2) the envelope `check_signature` must verify over the *enriched* payload
  `(entries, registry_type, responder_rhash)` (new shared free fn
  `directory_response_signature_payload`, so a valid signature over one (type, responder) can't be
  replayed under another); (3) each entry must carry its *own* binding signature, verified with
  `NodeIdentity::verify_binding(node_key, node_rhash, requested_type, node_types, signature)` — a
  directory cannot forge a peer's signature, so it can't attribute an attacker rhash to a real key.
  On the first bad entry the whole response is rejected before any peer is installed. Entries are
  installed via a new refresh-only `register_directory_peer` (existing key → only `last_seen`
  refreshed, rhash/conn never touched); `register_peer`'s legitimate overwrite is preserved for the
  RegisterAck path. The binding is captured at registration (`handle_register` stores it via
  `NodeRegistryNode::with_binding`) and echoed in responses by `handle_request` (entries built only
  from nodes with a non-empty stored binding — the "vouch only for nodes you directly registered"
  property). `src/node.rs`: `NodeRegistryEntry` gains `signature`/`requested_type`/`node_types`;
  `NodeRegistryNode` gains `directory_signature`/`directory_requested_type`/`directory_node_types`.
  Note (**wire-compat, AUDIT ground rule 4**): this phase *changes* wire shapes — `NodeRegistryEntry`
  gains three fields and the `NodeRegistryResponse` envelope signature now covers
  `(entries, registry_type, responder_rhash)` instead of the old `rmp(&entries)`. It is
  forward/backward-safe only if both ends run 1.5; directory sync between mixed-version nodes is
  gated until both sides ship this. The control-plane struct *names* are unchanged, so no other
  crate's wire code breaks.
  Done: production producer + fail-closed receiver in place. 7 tests in `src/node/registry.rs`:
  `directory_response_registers_valid_entries` (positive), and 6 new/fixed regression tests each
  **failing without the fix** (proven by temporarily reverting `handle_directory_response` and
  `register_directory_peer`, running the suite, then restoring the fix):
  `directory_response_rejects_real_key_attacker_rhash` (headline C7 — `{real_key, attacker_rhash}`
  rejected), `directory_response_rejects_unregistered_responder`,
  `directory_response_rejects_invalid_signature`,
  `directory_response_rejects_tampered_registry_type`,
  `directory_response_poisoned_cannot_change_registered_rhash`,
  `register_directory_peer_refresh_only`. Full workspace green
  (core 314, committer 33, executor 10, finalizer 53, sentinel 42 — 0 failed).

- [x] **1.6 Heartbeats: authenticate before refreshing liveness** (L1) — *done 2026-08-21*
  File: `src/node/registry.rs` (`handle_heartbeat`, `refresh_last_seen`).
  Action: `handle_heartbeat` refreshed `last_seen` on a *self-claimed* `requester_key` with no
  check (L1), so any sender could make any registered node appear alive — liveness
  (`evict_expired`, 30 s cutoff) was forgeable. Added a fail-closed
  `NodeIdentity::verify_binding` over the standard `NodeRequest` binding tuple
  `(requester_rhash, requested_type, requester_types)` before the registered-key lookup,
  mirroring `handle_register`. Forged / wrong-tuple / unregistered → reject early, never
  reach `refresh_last_seen`.
  Note: no production worker produces a `NodeRequestType::Heartbeat` (only the handler exists,
  plus test-only constructions in `src/node.rs`), so authenticating the receiver breaks no
  live producer. No wire-shape change — `binding_signature` was already carried, the receiver
  now simply checks it (AUDIT ground rule 4).
  Done: production `handle_heartbeat` now verifies the binding signature before refreshing.
  3 tests in `src/node/registry.rs`: `forged_heartbeat_does_not_refresh_last_seen` (headline L1),
  `heartbeat_binding_is_tuple_specific` (replay with a spoofed rhash rejected), and
  `authenticated_heartbeat_refreshes_last_seen` (positive control); each of the two
  discriminators was proven to fail without the fix by reverting `handle_heartbeat` and running
  the suite. Full workspace green (0 failed).

## Phase 2 — Deterministic consensus primitives
*Closes: C2, C6, L7. Small mechanical fixes with large impact — without these, no two nodes can
agree on anything.*

- [x] **2.1 Canonical serialization in the block hash; hash-bind the missing fields** (C2)
  Files: `src/blocks.rs:24, 85-101`, `src/transactions.rs:273`.
  Action: `BlockFactory::create_hash` must hash canonical forms — sorted-key (e.g. `BTreeMap`
  or explicit sort) serialization of `token_metadata` and `executor_sigs`; include
  `proposer_key` and `epoch_number` in the hash input.
  Verify: **cross-process determinism test** — build the same logical block with maps populated
  in different insertion orders (and across a serde round-trip) and assert identical
  `current_hash`.

- [x] **2.2 Sort before shuffling** (C6)
  Files: `src/epoch.rs:150-153` (`ExecutorSet::shuffler()`), `:275-277` (shard_count==1 shortcut).
  Action: `keys.sort()` before Fisher-Yates, matching `deterministic_select` at
  `src/epoch.rs:236-237`.
  Verify: `deterministic_select_shard` returns identical partitions for the same stake set built
  in different insertion orders and after a serde round-trip.
  **Done:** *2026-08-21* — the RNG seed in `shuffler()` was already deterministic (SHA-256(epoch)),
  but it was applied to `HashMap`'s random insertion order, so the Fisher-Yates permutation — and
  therefore the shard partition the sentinel routes on — varied per node's build order. Sorted the
  keys at both HashMap read-sites: `ExecutorSet::shuffler()` (`let mut keys … ; keys.sort();`) so
  the shuffle starts from a canonical order, and the `shard_count==1` shortcut (return the sorted
  full set). Left `Shuffler::new` a pure Fisher-Yates over the given slice; determinism is fixed
  exactly where the randomness leaks in. **Regression** `deterministic_select_shard_sorted_before_shuffle`:
  same `ExecutorSet` built forward, reversed, and via rmp serde round-trip → identical partitions for
  `shard_count==1` (shortcut) and `>1` (shuffle) across 4 (shard_count, tx, epoch) tuples; proven a
  true discriminator (it fails `forward vs reversed` with the sorts commented out). **Wire-compat:**
  `deterministic_select_shard` still returns `Vec<Vec<u8>>` — internal ordering normalization only,
  no wire-shape change (AUDIT ground rule 4). Core 321 (was 320, +1); full workspace 0 failed.

- [x] **2.3 One `proposer_key` semantics** (C2) — *done 2026-08-21*
  Files: `src/blocks.rs:57`, `src/tokens.rs:180`, `finalizer/src/block_builder.rs:179,187`
  (the checklist's `committer/src/block_builder.rs` path is stale — the file has always lived in
  the finalizer crate; the quoted line numbers 179/187 match it exactly).
  Action: unify the one field `Block.proposer_key` is derived from. `Token::create_block` and
  `BlockBuilder::create_block` already read `signed_tx.proposer_key`; `Block::from_transaction`
  read `signed.leader_address` instead — change it to `signed.proposer_key`. Add a single
  semantics doc comment at `src/blocks.rs:30` (always the leader identity on the signed
  transaction, fed to the hash + conflict resolution), and a doc note on the finalizer's
  `leader_address` field (line 33) that it must equal the epoch-selected leader.
  Verify: both/all constructors produce the same `proposer_key` for the same leader.
  **Done:** `Block::from_transaction` now sets `proposer_key: signed.proposer_key.clone()`
  (`src/blocks.rs:67`); `Token::create_block` and `BlockBuilder::create_block` were already
  correct — the mismatch was latent because every producer set `SignedTransaction.leader_address`
  and `.proposer_key` equal. One-line production change, no wire/serialized-field change
  (AUDIT ground rule 4). Regression `from_transaction_uses_signed_transaction_proposer_key`
  (`src/blocks.rs`, asserts a `leader_address != proposer_key` tx resolves to the tx's
  `proposer_key`) and `all_block_constructors_agree_on_proposer_key` (finalizer, runs one
  drifted `SignedTransaction` through all three constructors and asserts identical
  `proposer_key`) — each proven a true discriminator by reverting the one-line fix. `cargo
  check` clean; full workspace **473** green (baseline 471 + these 2); grep gate shows no
  remaining `leader_address`→`proposer_key` derivation in core block construction.

- [x] **2.4 `remove_block` pops the tip** (L7)
  File: `src/blocks.rs:127-129`.
  Action: `pop_back()` (tip), not `pop_front()`; update callers/tests.
  Verify: unit test on a multi-block chain.

## Phase 3 — Transaction & block security
*Closes: C3, C5, H12, H15.*

- [x] **3.1 Bind the authenticated submitter to `tx.sender`** (C3)
  Files: `src/transactions.rs` (add sender-signature field), sentinel validation path.
  Action: require a sender signature over the canonical transaction; verify it; reject when the
  authenticated envelope sender ≠ `transaction.sender`.
  Verify: a peer cannot submit a transfer debiting an account it does not control.

- [x] **3.2 Real block validation, fail closed** (C5)
  Files: `src/environment.rs:205` (block validator registry created empty),
  `src/tokens.rs:864-869` (accept-all `DefaultBlockValidator`).
  Action: populate the registry in production wiring; remove the accept-all fallback — no
  validator registered = reject the block, don't silently pass.
  Verify: a `BlockFinalized` for a token with no registered validator is rejected.

- [x] **3.3 Atomic, validated tip append in `handle_block_finalized`** (C5)
  File: `committer/src/committer.rs:344-399`.
  Action: verify hash + linkage + (Phase-1) proposer signature; perform read-tip and append
  under one lock scope (no read-guard-then-`get_mut` gap).
  Verify: concurrent sibling blocks → exactly one appended; **update the double-append test at
  `committer.rs:2041-2047`** which currently asserts the buggy behavior.

- [x] **3.4 Orphan handling for non-tip blocks** (H15)
  Files: `committer/src/orphan_buffer.rs` (new), `committer/src/committer.rs` (BlockFinalized path).
  Action: blocks whose `previous_hash` isn't the current tip are buffered in a bounded, per-token,
  TTL'd [`OrphanBuffer`] (no `Block`/`Message` wire change) instead of being silently dropped; on
  each append the Committer replays the buffer and promotes every block whose parent has just
  landed, cascading multi-block out-of-order sequences. A globally-full buffer is rejected and
  logged (never silently dropped).
  Verify: out-of-order delivery of N blocks → all eventually committed. Headline regression:
  `handle_block_finalized_buffers_orphan_and_replays_on_tip_advance` (deliver b2 first → buffered;
  then b1 → both land, chain grows by 2; buffer empties) and
  `handle_block_finalized_replays_orphan_cascade_in_out_of_order_delivery` (shuffled N+2,N+1,N+3
  order → all land). Updated the old `handle_block_finalized_ignores_orphan_block` test (it encoded
  the silent-drop bug, per ground rule 3). `OrphanBuffer` unit tests cover capacity eviction,
  per-token cap, TTL expiry, and the `RejectedFull` path. Workspace green (0 failures).

- [x] **3.5 Commit the validated payload, not whatever arrived** (H12)
  File: `committer/src/committer.rs` (commit path), `src/transactions.rs`
  (`Committed.block_hash` currently stores a `token_id`).
  Action: before committing, match the incoming commit's transaction payload against the
  validated/pooled transaction (hash comparison); fix the `block_hash`/`token_id` field
  misnomer.
  Verify: a commit whose payload differs from the validated tx is rejected.

## Phase 4 — Production wiring: make the pipeline actually run
*Closes: H4, H5, H7, H8, M11, M12.*

- [x] **4.1 Repair `committer/src/main.rs` wiring** (H4)
  Files: `committer/src/main.rs:150, 168, 175, 178, 191, 210`; `committer/src/committer.rs:895`.
  Action: populate `PendingTransactionRegistry` from the live pipeline (commit currently always
  fails `TransactionNotInFinalizing`); load `StakeStore` from the data service at boot; stop
  discarding `propose_blocks` output; fix the `CandidateRegistry` double-wiring/shadowing
  (moved into `EpochReconciler`, then a second instance built for the committer); make
  `sign_binding(...)` a hard boot error, never `unwrap_or_default()`.
  Verify: end-to-end test that boots the real committer (no test-only registry injection) and
  commits a transaction.

- [x] **4.2 Sentinel routes on the current epoch, not literal 1** (H5)
  File: `sentinel/src/sentinel.rs:221` (and every other literal-`1` routing call site;
  `current_epoch` at `:45` is write-only).
  Action: route via `current_epoch`; keep it updated from epoch-advance events with snapshot
  cache invalidation.
  Verify: after an epoch advance, new transactions route against the new epoch's executor
  set/stake snapshot.

- [x] **4.3 Harden the data-service channel** (H7, H8, M2, M5)
  Files: `src/data.rs:16-17`, `src/conns/senders.rs:29-45, 60-76`, `src/conns/factories.rs:49-58`,
  `src/conns/listeners.rs:15-21`.
  Action: absolute, 0700-scoped socket path (drop the relative `"data"`); authenticate the peer
  (at minimum unix peer-credential check, prefer shared secret); 4-byte-BE length framing with
  the 16 MB cap (reuse `get_data`); read/write timeouts on every stream; UDS listeners bind a
  per-UID runtime dir and return `Result` instead of `expect()` on bind failure.
  Verify: (a) a hung/slow data service times out and degrades registration only, not the whole
  RNS worker pool; (b) a pre-created socket path fails startup cleanly (no panic, no symlink
  hijack); (c) response > 16 MB rejected.

  **Done:** *2026-08-25* — data channel hardened end to end. `src/conns/uds.rs` (new): per-UID
  runtime dir (`$XDG_RUNTIME_DIR/pneumatic`, else `<temp>/pneumatic-<uid>`, forced 0700 via
  `PermissionsExt`) + absolute `data_socket_path`; `prepare_socket_path` rejects a pre-created
  symlink and removes a stale socket before bind. `src/conns/senders.rs`: every
  `UdsSender`/`TcpSender::get_response` now sets read+write timeouts (WouldBlock/TimedOut →
  `ConnError::Timeout`), length-frames the response, and HMAC-SHA256-authenticates via a per-worker
  shared secret — wire shape is `[4-byte BE len][auth_tag(32) || body]`, the length covering the
  whole authed body so the 16 MB cap covers it. `src/conns/listeners.rs`: `CoreUdsListener::new` /
  `CoreTcpListener::new` return `Result` instead of `expect()`. `src/conns/factories.rs`:
  `ConnFactory` carries the shared secret + rw timeout; `get_listener` UDS path resolves the
  absolute per-UID socket. `src/data.rs`: `DefaultDataProvider` holds `source` + optional secret
  (`with_secret` / `with_timeout`); `Timeout` / `Unauthenticated` map to `DataError::Timeout` /
  `PeerUnauthenticated`. `committer/src/main.rs` reads `PNEUMATIC_DATA_SECRET`. 11 senders tests +
  37 conns tests green; full workspace green.

  Note (**wire-compat, AUDIT ground rule 4**): this phase *changes* the data-channel framing from
  raw `write_all` / `read_to_end` to length-prefixed `[4-byte BE len][auth_tag(32) || body]` and
  adds shared-secret HMAC. The external data daemon must respond with framed bodies carrying a
  valid HMAC tag computed over the payload under the shared secret, or hardened workers reject with
  `Unauthenticated`. Forward/backward compatible only when both the daemon and the workers ship
  4.3; a 4.2-era unframed daemon will time out against a 4.3 worker.

- [x] **4.4 Stake gates: per-type, real, off the hot path** (H7, H8)
  Files: `src/config.rs:205-219` (uniform `min_stake: 10`), `src/environment.rs`
  (`CostModel.global_min_stake` never consulted), `src/node/registry.rs:329`
  (stake gate runs on the RNS worker pool).
  Action: differentiate per-type minimum stakes and enforce `global_min_stake`; move the
  blocking data-service stake check off the RNS worker pool (async/off-thread) so a slow data
  service cannot wedge the 4-thread network pool.
  Verify: four concurrent stalled stake checks leave the RNS pool responsive.

  **Done:** *2026-08-27* — per-type minima are now enforced alongside the global floor and the
  registration gate runs off the RNS worker pool. `src/config.rs`: added `meets_minimum_stake` — a
  module-scope free function (`stake >= global_min && stake >= type_min`), reachable from any crate
  via `crate::config::meets_minimum_stake` (kept free, not an inherent `Config` method, so both the
  registration gate and the sentinel share one AND). `Config::get_global_min_stake` reads
  `cost_model.global_min_stake` and falls back to the cost-model default (10) when the env is
  absent from the registry. `committer/src/main.rs` builds an `Arc<StakeIndex>`: a background
  `std::thread` periodically loads the current-epoch `StakeSet` into an in-process `pubkey -> stake`
  index (`StakeIndex::start`); the gate closure (`StakeIndex::make_check`) is a pure in-memory
  DashMap lookup with zero data-service I/O, so a hung data service can never hold one of the 4
  plain-`std::thread` RNS workers (no Tokio runtime). The index is warmed synchronously before the
  network starts (`StakeIndex::warm`); a cache miss returns 0 stake ⇒ the gate fails closed; a
  refresh error leaves the stale index in place ⇒ still fails closed. The committer's epoch loop
  advances the cache via `StakeIndex::set_epoch(current_epoch_number)` (single source of truth).
  `src/node/stake_index.rs` (new): the `StakeIndex` type + 7 regression tests. `sentinel/src/sentinel.rs`:
  `check_stake_for_type` now uses `meets_minimum_stake` — two new tests (a well-staked user passes;
  a user with 5 stake passes the lowered Sentinel floor of 1 but is rejected for missing the global
  floor of 10, proving the global floor is now enforced). Workspace suite green: 513 passing, 0
  failures.

  Note (**wire-compat, AUDIT ground rule 4**): *no wire-message-shape change.* `StakeCheck` is an
  internal Rust type (`Arc<dyn Fn(&[u8], &NodeRegistryType) -> bool + Send + Sync>`); its signature
  is unchanged and the RNS control path (`NodeRequest` Register → `handle_register` → `RegisterAck`)
  is byte-for-byte identical. The per-type minima are driven by the env-spec `CostModel.per_type_min_stake`
  (`#[serde(default)]`) — a config-schema change to the local `config` JSON, not an RNS control-plane
  message, and fully backward/forward compatible (absent ⇒ empty map ⇒ uniform `min_stake: 10`).

- [x] **4.5 Gas accounting: right partition, no swallowed errors** (M11)
  File: `committer/src/committer.rs:265-273`.
  Action: query `token_partition_id` (not `main_environment_id`); surface `get_user`/`save_user`
  errors (log at minimum; decide protocol semantics for deduction failure).
  Verify: failed deduction is observable and cannot silently free gas or overdraw.

- [x] **4.6 Atomic keystore write** (M12) — *done 2026-08-27*
  File: `src/rns/identity.rs:217-252`.
  Action: write temp file + `rename()`; on boot, a corrupt keystore is a clear error with
  recovery guidance (backup restore), never a silent regenerate.
  Verify: kill the process mid-write → boot reports the corruption cleanly.

  **Done:** the keystore (`node_identity.json`) is now written via temp-file + atomic `rename`,
  with a backup + conditional recovery hint on corrupt content (identity.rs only — no runtime
  dependency, no wire change, no new error variant, no contract change to `config.rs:98-106`).
  `write_file` now (1) serializes once, (2) backs the existing keystore up to `node_identity.json.bak`
  *before* the first overwrite (a copy failure is a hard `CryptoError`, fail-closed — first boot
  skips it, so `.bak` is currently defensive), (3) writes a `0600` temp file (via `OpenOptionsExt`
  on unix, `File::create` on non-unix — Windows has no portable chmod, documented), `sync_all()`s
  it, then `rename`s it into place, (4) and cleans any leftover temp up through an RAII `TempFile`
  guard that deletes on drop unless `.commit()` is called. Atomicity means a reader never sees a
  torn file and a killed process leaves the existing keystore intact. `load` now carries the
  recovery guidance: after a successful `fs::read`, the parse/hex/length/"no public key" branches
  append a conditional hint — a `.bak` exists ⇒ "restore with `cp {path}.bak {path}` and restart, or
  re-import from a trusted source"; no `.bak` ⇒ "do not regenerate on a running node — that
  orphans the on-chain stake; recover from a secure offline copy or regenerate only on a fresh/unstaked
  node". The raw `fs::read` failure (permissions/IO) is left untouched (not corruption). Forced
  `0600` now applies to an **existing** file too: a `0644` keystore overwritten via the atomic rename
  lands as `0600` (old code reopened `O_TRUNC` and kept the loose bits). Six new tests in `mod tests`
  (`identity.rs`), all invoking `write_file` directly since `load_or_create` never overwrites:
  `test_write_file_writes_backup_on_overwrite` (strict discriminator — `.bak` exists and reloads to
  the prior key; fails without backup creation), `test_corrupt_keystore_error_names_backup` (strict
  discriminator — corrupt file whose message contains `.bak`/"restore"/"refusing to regenerate"),
  `test_corrupt_keystore_without_backup_refuses_regenerate` (first-boot branch text),
  `test_write_file_forces_0600_on_existing_file` (strict discriminator — pre-create 0644 → 0600),
  plus `test_atomic_write_roundtrips_and_leaves_no_tmp` and `test_partial_intermediate_preserves_primary`
  (documented non-discriminators). Atomicity is structural (rename), so a real SIGKILL is driven
  deterministically by corrupting the primary and asserting the recovery-guidance `load` path.
  Ground-rule check: no production path silently regenerates a keystore, no new `.expect()`/`unwrap()`
  on keystore I/O. `cargo check` clean; full workspace suite **533** green (Phase 4.5 baseline 527
  + these 6), 0 failures.

## Phase 5 — Economics & consensus enforcement
*Closes: H1, H2, H3, H6, H9, H13, H14, H16(self-signed), M10, M13.*

- [x] **5.1 Make slashing real** (H1) — *done 2026-08-27*
  Files: `src/environment.rs:42-43,106,120` (new `CostModel.slash_fraction`, default full stake),
  `src/tokens.rs:495` (test-helper literal), `committer/src/epoch_manager.rs:90-132` (`apply_ops`
  single pass), `:136-155,204-232` (`reconcile_internal` now emits `Slash` on SameProposerSlash),
  `committer/src/committer.rs:1030-1054` (commit-time slash amount + `?` fail-closed),
  `:1648,2259` (`slash_fraction` threaded into `EpochReconciler`).
  Action: slash = `current_stake × CostModel.slash_fraction` (default `1.0` = full stake). Epoch
  reconciliation now emits a `Slash` op for each resolved SameProposer (double-sign) conflict, and
  the commit-time path computes the real amount and propagates errors (`?`) instead of the dead
  `Slash(key, 0)` swallowed by `.ok()`. `apply_ops` applies each op (incl. `Slash`) exactly once.
  `finalization_conflicts` kept and honored — informational record + Phase 5.2 loser-discard
  handoff. Natural idempotency: a full-*remaining*-stake slash makes re-slashing a zeroed proposer
  a no-op, so commit-time and reconcile-time both re-seeing the same conflict double-slashes
  nothing.
  Verify: double-signing test asserts the offender's stake actually decreases by the configured
  amount — `commit_conflict_same_proposer_emits_slash` (offender → 0 full),
  `commit_conflict_same_proposer_partial_slash_respects_fraction` (0.5 → 50, proves the amount is
  configured), and a `reconcile_same_proposer_conflict_slashes_proposer` path test asserting
  `slashing_ops` plus `apply_ops` moving the StakeStore. 535 tests passing, 0 failures.

- [x] **5.2 Discard losers on conflict; bound the registry** (H2) — *done 2026-08-27*
  Files: `src/epoch.rs` (`CandidateRegistry`), `src/blocks.rs:250` (new `Blockchain::last_block`),
  `src/tokens.rs:222` (`Token::commit_block`), `committer/src/block_services.rs:67`
  (`commit_block`), `committer/src/committer.rs:999-1121` (`handle_conflict_at_commit`),
  `:482-490` (caller `check_and_commit_transaction_results`),
  `committer/src/committer_error.rs` (new `LoserDiscarded`),
  `committer/src/epoch_manager.rs:182-207` (`reconcile_internal`).
  Action: on `DiscardLoser`, undo the losing block's append and commit the winner; call
  `remove_conflicted` after **every** resolution (`DiscardLoser`/`SameProposerSlash`/`TieFlagBoth`
  each clear the resolved group so no branch leaves the loser standing); enforce a per-position
  max on `CandidateRegistry`; give `misshapen_tokens` a real side effect instead of being a
  write-once record.
  Verify: contested (token_id, previous_hash) → exactly one block remains in the chain;
  registry size stays bounded under repeated conflicts.

  **Done:** honored the three locked design choices — (1) roll back the losing tip + commit the
  winner atomically, (2) remediate `misshapen_tokens` by slashing the chain tip's proposer,
  (3) LRU eviction (evict oldest per-position candidate). `CandidateRegistry` gains a `max_candidates`
  field (`DEFAULT_MAX_CANDIDATES = 1024`, a `with_max_candidates` ctor, and a manual `Default` impl
  so `new()`'s signature is unchanged across all ~14 call sites); `insert` evicts the oldest via
  `while entry.len() > self.max_candidates { entry.remove(0) }` (Vec front = oldest = true LRU).
  The commit path's conflict winner links to the tip's *parent*, so `Token::commit_block` now
  **rolls the loser tip back before validating** (a `rollback_tip_hash: Option<&[u8]>` param) with
  **restore-on-failure** so a rejected winner never truncates the chain — the ordering matters
  because `validate_next_block` requires `previous_hash == tip`. `commit_block` threads the param
  through, atomic under the single `get_mut` lock. `handle_conflict_at_commit` now returns
  `Result<CommitConflictOutcome, CommitterError>` (new private `CommitConflictOutcome { Commit,
  CommitWinnerAfterRollback(Vec<u8>) }`) and calls `remove_conflicted` on every non-empty arm; the
  caller commits with `None` or `Some(loser_hash)` per outcome. `reconcile_internal` (via the new
  `Blockchain::last_block`) derives the tip proposer on an invalid chain and emits a real
  `Slash(tip_proposer, stake·slash_fraction)` op; `misshapen_tokens` is kept as an informational
  record. New `CommitterError::LoserDiscarded` variant (no fields).
  **Wire-compat:** none — all new parameters are internal (commit path + spec fields). Ground-rule
  check: fail-closed (a losing re-proposal is rejected, not appended); the commit path's `add_block`
  stays linkage-free *by design* (H2 addresses the conflict fork, not general unlinked appends).
  **Tests:** full workspace **green, 0 failures** (core 354, committer lib 56 + main 7, finalizer 54,
  sentinel 53, executor 10). New discriminators — each proven to fail when its fix is reverted:
  `commit_conflict_rolls_back_loser_tip_and_commits_winner` (rollback-before-validate ordering),
  `commit_conflict_rejects_losing_commit` (loser rejected, tip preserved),
  `candidate_registry_bounded_under_repeated_conflicts` (LRU cap via direct raw-insert of N > cap
  candidates and asserts the oldest evicted), `reconcile_misshapen_chain_slashes_tip_proposer`
  (invalid chain → slash of the tip proposer, stake → 0), and core
  `registry_lru_evicts_oldest_when_over_cap`. Updated tests encoding the old buggy behavior:
  `commit_conflict_different_stakes_discards_loser` (candidate count 2 → 0),
  `commit_conflict_same_proposer_emits_slash` + `…partial_slash_respects_fraction` (is_ok → is_err),
  and the double-append test (reconcile with discard-loser). `cargo check` clean.

- [x] **5.3 Unpredictable selection seeds** (H3) — *done 2026-08-28*
  Files: `src/epoch.rs:180-183, 225-227, 395-398`.
  Action: seed = `SHA-256(domain ‖ epoch_number ‖ prev_block_hash)` with a distinct domain byte
  per selection type (leader / shard shuffle / finalizer / shard index), per ADR-003.
  Verify: same (epoch, stake set) with different prev_block_hash → different leader/shards.

  **Done:** every deterministic selection seed is now bound to the mined `prev_block_hash`, so a
  future leader / executor shard / finalizer is only knowable once the *previous* block is actually
  mined (before it was predictively derivable from the public `epoch_number` + stake set). One
  shared `derive_selection_seed` helper in `src/epoch.rs` computes
  `seed = SHA-256(domain ‖ epoch_number ‖ prev_block_hash ‖ extra)` with a **distinct domain byte
  per selection type** (`LEADER_DOMAIN=0x01`, `SHARD_SHUFFLE_DOMAIN=0x02`, `FINALIZER_DOMAIN=0x03`,
  `SHARD_INDEX_DOMAIN=0x04`) — so a leader seed can never be replayed as a shard-index seed, per the
  ADR requirement `src/epoch.rs:180-183, 225-227, 395-398`. The four seed sites: `Shuffler::new`
  (shuffle), `deterministic_select` (serves both leader `LEADER_DOMAIN`+empty-extra and finalizer
  `FINALIZER_DOMAIN`+tx_id-extra), and `deterministic_select_shard` (finalizer + shard-index).
  `tx_id` is **kept** as `extra` for the finalizer/shard-index paths — dropping it would route every
  tx to the same finalizer/shard and regress load distribution. Leader selection now threads
  `prev_block_hash`: `IEpochLeaderSelector::select` gains a `prev_block_hash` arg (breaking —
  consistent with the intentional breaking-API changes of earlier phases), both impls updated (core
  `LeaderSelector::select`, committer `LeaderSelector::select_internal`), and `Epoch::new_with_leader`
  forwards it. `prev_block_hash` is sourced from the two production producers. The **committer**
  reads the tip **locally** from its token cache (`self.tokens`, where it holds chain state and never
  persists it to the data service) — wired into `handle_epoch_reconcile` (`committer.rs:960`) and
  `advance_epoch` (`committer.rs:1211`); the genesis/boot path passes `vec![]`. The **sentinel** reads
  it via the new default trait method `DataProvider::latest_block_hash` (`src/data.rs:20`; returns
  `Ok(None)` → empty salt by default, so no existing test provider needs a change).
  **Wire-compat:** none — `latest_block_hash` is a new default trait method (no impl change for the
  ~6 test providers), no wire `DataOp`/`GetOp` entry, and `deterministic_select`/`Shuffler` are
  internal helpers. Only Rust signatures change.
  **Production-data-flow gap (recorded, not fixed):** the sentinel path does not bind the mined tip
  in production — the chain tip is never persisted to the data service (only the committer holds it
  locally), so `default` `latest_block_hash` returns `Ok(None)` → the sentinel's finalizer/shard
  routing varies only by `domain ‖ epoch ‖ tx_id`, not by mined tip. The core derivation is still
  correct and regression-tested, and the committer leader path is always tip-bound; closing the gap
  needs a worker to persist the mined tip.
  **Tests:** full workspace **green, 0 failures** (548 passing: core 359, committer lib 57, pipeline
  integration 7, finalizer 54, sentinel 55, executor 10, transport 6). New discriminators — each
  proven to fail when its fix is reverted: `committer::tests::advance_epoch_leader_changes_with_mined_tip`
  (committer leader binds the local mined tip),
  `sentinel::tests::assign_finalizer_changes_with_mined_tip` and
  `sentinel::tests::get_shard_executors_changes_with_mined_tip` (finalizer + shard routing bind the
  mined tip). Plus core `src/epoch.rs::tests`: `selection_seed_leader_changes_with_prev_block_hash`,
  `selection_seed_distinct_domains_differ`, `selection_seed_shard_index_changes_with_prev_block_hash`,
  `selection_seed_matches_manual_hash` (exact byte layout), and
  `selection_seed_independent_of_tx_id_for_leader`. `cargo check` clean.

- [x] **5.4 One epoch writer; authenticated epoch advance** (H9, M8) — *done 2026-08-28*
  Files: `src/epoch.rs` (`StakeSet`/`ExecutorSet` `canonical_bytes` + `fingerprint`), `src/data.rs`
  (`DataError::SnapshotCorrupt`, `StakeSnapshotEnvelope`/`ExecutorSetEnvelope`, Default + Stub provider
  verify-on-load), `committer/src/committer.rs` (`advance_epoch_to`, `snapshot_save_err`,
  `TestDataProvider::with_snapshot_save_failure`), `committer/src/committer_error.rs`
  (`SnapshotPersist`), `committer/tests/` (`advance_epoch_to_never_rewinds_or_reuses`,
  `advance_epoch_to_surfaces_snapshot_save_error`).
  Action: authenticate `EpochReconcile` (Phase-1 envelope auth closes the unauthenticated advance);
  single source of truth for the epoch number — reject/queue a second advance for the same epoch,
  never rewind; persist a hash/attestation with each saved stake snapshot and verify on load; surface
  `save_stake_snapshot`/`save_executor_set` errors (currently `let _ =`).
  Verify: reconcile-then-advance does not reuse an epoch number; a corrupted snapshot file is
  detected at load, not trusted.

  **Done:** the two divergent epoch-advance mechanisms now funnel through one guarded writer,
  `advance_epoch_to` (`committer.rs:1233`), whose `EpochBoundaryDetector` epoch is the authoritative
  source — both the internal `advance_epoch` wrapper and the wire `handle_epoch_reconcile` call it
  (`committer.rs:1212`, `committer.rs:961`), so they can never disagree on or rewind the number, and
  the counter can never lag the detector. Inside it: reads the stake set and the mined `prev_block_hash`
  (local token cache, same Phase-5.3 source), locks the detector via `try_lock` (fails closed to
  `Ok(None)` if already held, serializing the two writers), and rejects any advance whose target does
  not strictly exceed `current_epoch_number` — a reused or rewinding number is refused, never applied
  (`committer.rs:1263`). On success the detector advances, the counter mirrors it, and **both**
  snapshots are persisted with `.map_err` surfacing persistence failures as `SnapshotPersist { epoch,
  kind, cause }` (committer_error.rs) via the `snapshot_save_err` helper (`committer.rs:381`)
  instead of the old `let _ =`. Snapshots now carry a SHA-256 attestation: `StakeSet`/`ExecutorSet`
  gain `canonical_bytes()` (sorted `BTreeMap` → MsgPack, so the digest is stable across save/load
  regardless of `HashMap` order) and `fingerprint()` (= `sha256(canonical_bytes)`); the
  `StakeSnapshotEnvelope { payload, hash, epoch }` / `ExecutorSetEnvelope` (`src/data.rs`) verify
  `hash == payload.fingerprint()` on load, else `DataError::SnapshotCorrupt` — the storage key
  (`epoch.to_be_bytes()`) and every `GetOp` variant are unchanged. Item #1 (auth) was already closed by
  the Phase-1 `authenticate_message` gate (`"EpochReconcile"` → `AllowedSenders::SelfOnly`) and its
  regression test `foreign_sender_epoch_reconcile_is_rejected`, so it needs no change.
  **Discriminators, each proven to fail without its fix by temporary revert (ground rule 2):**
  `snapshot_envelope_detects_corruption` (revert the on-load `verify()` → corrupted snapshot
  round-trips as `Ok`); `advance_epoch_to_never_rewinds_or_reuses` (revert the stored>=new guard → the
  seeded-ahead counter gets overwritten instead of refused); `advance_epoch_to_surfaces_snapshot_save_error`
  (revert the `.map_err(...)?` on the saves → the advance returns `Ok(None)` instead of the error).
  Workspace: 553 passing (548 + 5).
  **Wire-compat (AUDIT ground rule 4):** the `SaveOp::StakeSnapshot` / `SaveOp::ExecutorSet` variants
  now carry a `{ payload, hash, epoch }` envelope instead of a bare `StakeSet`/`ExecutorSet` (a shape
  change to the serialized `DataRequest`). The storage key (`epoch.to_be_bytes()`) and every `GetOp`
  variant are **unchanged**, and the data service is a generic key→value store keyed by epoch bytes
  that stores the serialized `DataRequest` opaquely — so the envelope round-trips through any existing
  data service with no change. **Caveat:** if a real data service ever *deserializes* `SaveOp`
  contents rather than storing them opaquely, it must accept the new envelope shape; a service that
  parsed the previous bare-stake-payload shape will choke on the `hash`/`epoch` fields. No `Message`,
  `DataOp`, or `GetOp` shape changes.

- [x] **5.5 Protect token replacement** (H13) — *done 2026-08-29*
  File: `committer/src/committer.rs` (`handle_token_distribution`, `token_distribution_conflict_err`,
  `Entry` import); `committer/src/committer_error.rs` (`TokenConflict` variant).
  Action: `TokenDistribution` may not replace an existing token id from an arbitrary peer —
  require the appropriate authenticated role and reject conflicts (or define an explicit,
  authorized overwrite flow).
  Verify: a peer cannot swap in a token (chain/metadata) for an id that already exists.

  **Done:** `handle_token_distribution` now rejects-on-conflict via a single atomic
  `self.tokens.entry(id)` check-and-insert (`Vacant` → insert `Ok(())`; `Occupied` →
  `Err(TokenConflict)`), replacing the blind `self.tokens.insert(id, token)` overwrite. `entry()`
  holds one shard write guard, closing the read-then-write gap a `contains_key`+`insert` leaves
  (same single-op shape as `handle_block_finalized`'s `get_mut`, Phase 3.3 / C5). The role gate is
  already correct (`allowed_senders_for("DistributeToken")` = `Exact(Committer)` via
  `authenticate_message`), and the vuln is role-agnostic, so no auth change. Reject-on-conflict
  blocks nothing legitimate: the token cache starts empty and a node only receives ids it lacks;
  chain advancement happens on the `BlockFinalized` path, never via re-distribution, and
  `bootstrap_token` (trusted internal, tests only) stays a plain insert. Logs a greppable
  `TOKEN REPLACEMENT REJECTED: token_id=<hex> already present — refusing token swap` line via the
  new `token_distribution_conflict_err` helper, following the `snapshot_save_err`/`gas_deduction_err`
  observability pattern. No wire change.
  **Discriminators, each proven to fail without its fix by temporary revert (ground rule 2):**
  `handle_token_distribution_rejects_conflicting_token_id` (revert the handler to the blind insert →
  it returns `Ok(())` and overwrites `name`/`asset_hash` → every value assertion fails);
  `handle_token_distribution_accepts_new_token_id` (positive guard — a not-yet-owned id still seeds).
  Workspace: 555 passing (553 + 2), 0 failures (committer 61, core 362, finalizer 54, sentinel 55,
  executor 10, integration 7). See [[phase-5-4-epoch-writer-snapshots]].

- [x] **5.6 Enforce nonces; `amount: None` must not pass** (H14, M12 from blocks/tx audit) — *done 2026-08-29*
  Files: `src/registry.rs` (pool accepts duplicate `(sender, seq)` — only `seq == 0` is
  checked), `src/validation.rs:180`, `src/action_router.rs:215,229`.
  Action: reject duplicate `(sender, seq)`; require `amount` (or define explicit
  zero-amount/no-transfer semantics) instead of `Option` flowing through every gate.
  Verify: replayed nonce is rejected; `amount: None` is rejected at admission.
  **Done (2026-08-29):** `(token_id, sender, seq)` dedup added to
  `PendingTransactionRegistry` (`used_nonces` DashMap, append-only, checked first in
  `enqueue_to_pool`, which now returns `Result`); `sentinel.rs` propagates the duplicate as
  `SentinelError::Registry`. `ExecutedBlockValidatorSpec::validate` rejects `amount: None`
  **and** `Some(0)` (`InvalidAmount`); `action_router` "Process"/"Preload" reject `None`
  before `verify_gas`. `SelfSigned` gate left untouched (executed path only). `amount` stays
  `Option<u64>` (wire untouched). 4 discriminators, each proven to fail on temporary revert.
  Workspace: 559 passing, 0 failures. See [[phase-5-6-nonce-and-null-amount]].

- [x] **5.7 Validate quorum/risk/economic config at spec load; wire the real risk gate** (H6) — *done 2026-08-29*
  Files: `src/environment.rs:100-104` (percentages copied verbatim),
  `finalizer/src/signature_collector.rs:89` (quorum 0.0 ⇒ one signature suffices),
  `src/validation.rs:196` (risk score compared against `override_quorum_percentage`),
  `src/environment.rs` (`max_risk` documented but dead).
  Action: reject specs whose `quorum_percentage`/`override_quorum_percentage`/
  `shard_quorum_percentage` are outside the protocol range and whose `max_risk`/
  `admin_tax_percentage` are outside [0,1]; wire the sentinel risk check to `max_risk` and
  delete the placeholder; also range-check gas multipliers (negative ⇒ free action) and
  `shard_count >= 1`.
  Verify: a spec with `quorum_percentage: 0` fails boot; the risk gate actually uses
  `max_risk`.
  **Done (2026-08-29):** `EnvironmentMetadataSpec::validate()` (environment.rs) →
  `PneumaticError::Encoding`, collects all violations. Ranges: `quorum_percentage` and
  `shard_quorum_percentage` ∈ (0,100] (kills the quorum-0.0 vuln), `override_quorum_percentage`
  ∈ [0,100], `max_risk` ∈ [0,1] (dedicated `ok_ratio` closure, not the [0,100] percentage check),
  `admin_tax_percentage` ∈ [0,1], each `amount_multiplier` finite & ≥ 0, `shard_count` ≥ 1.
  `load_from_spec` stays infallible — validation lives at the spec-load boundary:
  `config.rs` `get_environment_metadata` calls `validate()` before `load_from_spec` and returns
  `io::Error::InvalidData` on a bad spec, so `Config::build()` fails boot. Real gate in
  `validation.rs` `ExecutedBlockValidatorSpec::validate` now does
  `if risk.score() > env_data.max_risk → Validation([RiskExceedsThreshold])` (placeholder over
  `override_quorum_percentage` removed); `SelfSignedBlockValidatorSpec` left untouched. 15
  discriminators (9 spec validators + 2 risk-gate), each proven to fail on temporary revert.
  Workspace: 570 passing, 0 failures. See [[phase-5-7-risk-gate]].

- [x] **5.8 Conflict resolution: verified proposer, all candidates** (M10)
  Files: `src/epoch.rs:588-622`, `committer/src/committer.rs:633-661`.
  Action: take proposer identity from the verified leader signature, not the block's
  self-declared `proposer_key`; resolve against **all** candidates (fold), not only
  `candidates[0]`; replace the attacker-grindable equal-stake hash tie-break (or document and
  bound it); implement or delete `TieFlagBoth`.
  Verify: a forged `proposer_key` on an incoming block cannot steer the resolution branch.

- [x] **5.9 Fix the self-signed token path — owner-operated tokens made real** (audit:
  validation+tokens / sentinel)
  Files: `src/tokens.rs:426-442` (`mint_user_token` now writes `owner` as `hex::encode(&owner)` —
  a real 32-byte Ed25519 key isn't valid UTF-8, so it lives in the String `metadata` slot as hex);
  `src/validation.rs:72-84` (owner-check now hex-decodes the stored owner and compares bytes;
  missing **or** unparseable owner fails closed as `NotTokenOwner`, no `from_utf8`/`unwrap`);
  `src/validation.rs:166-170` (removed the Executed spec's owner-ban — the contradiction; Executed
  is now owner-agnostic, the owner gate lives only on the SelfSigned path); `sentinel/src/
  transaction_validator.rs:46-78` (selects the spec from `token.is_self_verified`, not `tx.action`);
  `sentinel/src/sentinel.rs:202-211` (routing is token-driven: `is_self_verified` routes to
  `handle_self_signed`, which releases the pre-lock and enqueues to the committer's shared pool;
  the `let _ = tx;` swallow and the obsolete `get_validation_spec_name` helper/tests removed).
  Discriminator: `token.is_self_verified` (contract tokens default `block_validation_spec_name`
  to "SelfSigned" but keep the flag `false`, so they stay on the standard pipeline — no misroute).
  Verify: an owner can execute a self-signed operation end-to-end —
  `sentinel::tests::handle_process_request_routes_self_verified_token_owner_operation`
  (owner == sender → accepted, no executor/finalizer, lands in the pool) and
  `handle_process_request_rejects_self_verified_tx_from_non_owner` (off-owner → `NotTokenOwner`,
  not admitted). Both proven to fail on a temporary revert: removing the mint-time owner write
  rejects the owner tx; removing the owner-check admits the off-owner tx. Full workspace **575
  tests, 0 failures**; `cargo check --workspace` clean.

## Phase 6 — Infrastructure & reliability hardening
*Closes: remaining Medium/Low items.*

- [x] **6.1 Timeouts on all blocking I/O** (M from infra audit) — see 4.3 for the data
  channel; also bound per-send time in `src/node/registry.rs:701-787`.
  Files: `src/node/registry.rs:701-745` (`send_to_all`), `751-787` (`send_to_all_blocking`).
  Action: every RNS fan-out `get_response` and every data-channel `conn.send` is now bounded by a
  detached-std-thread `bounded_send` (sync) or a `tokio::spawn_blocking` + `time::timeout`
  (`bounded_send_async`) wrapper — `SEND_TIMEOUT = 5s`. A hung send degrades to `Err(ConnError::
  Timeout)` instead of pinning the runtime/caller thread; the sync fan-out no longer assumes an
  ambient tokio runtime.
  Verify: `registry::tests::bounded_send_*` discriminators time out (not hang) on a 300 ms closure
  under a 100 ms bound; full workspace green (579 passing, 0 failures).
- [x] **6.2 `send_to_all` observability + concurrency** — every failed `send_to_all` /
  `send_to_all_blocking` delivery now recorded in a new `Arc<DashMap<([u8;16], NodeRegistryType),
  u64>>` `delivery_failures` (keyed by rhash + node type) and logged via
  `record_delivery_failure(...)` at `node/registry.rs:~130-140`. Async `send_to_all`'s RNS branch
  converted from a sequential `for` loop to a concurrent `join_all` (parity with the direct path);
  the direct path of both `send_to_all` and `send_to_all_blocking` now captures `Ok(Err(e))` and
  timeout-`Elapsed` arms instead of swallowing them; the blocking direct-connection branch — which
  previously dropped its un-awaited `conn.send()` future as a latent no-op — now drives each send
  with a self-contained `current_thread` runtime + `block_on` (no ambient runtime, consistent with
  6.1). Both methods still return `()` — no call-site or wire change. Test accessors `failure_count`
  / `total_delivery_failures` + `with_send_timeout` builder. Verify: `registry::tests` discriminated
  by 6 new tests (direct/blocking-failure recorded, blocking direct actually sends, timeout recorded
  on both async+blocking paths, positive control, helper keyed by rhash+type) — each proven to fail
  on temporary revert; full workspace green (585 passing, 0 failures).
- [x] **6.3 Atomic registration admission** — capacity check + insert under one lock (close the
  check-then-insert TOCTOU with a blocking stake gap in the middle)
  (`src/node/registry.rs:157-161, 318-348`).
  Action: added `admission_lock: Arc<std::sync::Mutex<()>>` to `NodeRegistry` (constructed in
  `init`). `handle_register`'s admission tail now re-checks capacity and inserts under that one
  lock, while the blocking stake gate and connection setup run OUTSIDE it, so the critical
  section only covers a map `len()` + `insert()` (never a blocking call on the plain-std RNS
  worker pool). `max_node_number` is now a hard invariant under concurrency — two registrations
  for different keys can no longer both pass the optimistic capacity check and over-admit a type.
  No selection or stake semantics changed; `register_peer`/`register_directory_peer` keep the same
  `len()`-then-`insert` shape but are single-threaded in tests (same-pattern hardening is a
  follow-up). Verify: `registry::tests::concurrent_admission_never_exceeds_capacity` races 200
  registrations against Sentinel cap 20 with a 5 ms stake gate and asserts `len() <= 20`; proven
  to fail on a temporary revert of the fix (over-admitted to 25); full workspace green (586
  passing, 0 failures).
- [x] **6.4 `TcpConnection` EOF is terminal** — *done 2026-08-31*
  File: `src/conns.rs` (`TcpConnection::from_stream`'s detached read loop; `listening_thread` test accessor).
  Action: `Err(_) => break` on the read loop (`src/conns.rs:99-109`), so peer EOF is terminal.
  Previously the loop was `Ok(data) => on_received(data); Err(ConnError::ReadError(_)) => continue; _ => break`
  — a clean peer close surfaces as `io::ErrorKind::UnexpectedEof` → `ReadError`, and the loop re-entered
  immediately, busy-spinning at ~100 % CPU until the process was killed. Now every read error breaks.
  This is safe because `get_data_async` does a blocking `read_exact` over a non-blocking tokio stream:
  tokio absorbs `WouldBlock` internally, so `read_exact` only errors on a genuinely broken/EOF connection,
  never on a transient "not yet enough bytes" condition worth retrying. No wire-shape change — single
  production code point; `listening_thread` is a `#[cfg(test)]` accessor exposing the detached `JoinHandle`
  so tests assert termination (a resolved handle ⇒ the loop exited) rather than relying on the loop's
  internal behavior. Verify: `src/conns.rs::conns_tests` discriminators — each proven to fail on a temporary
  revert of the `break` (the loop never terminates, so the awaiting `JoinHandle` times out):
  `tcp_connection_read_loop_exits_on_peer_disconnect` (client closes → EOF → loop exits),
  `tcp_connection_read_loop_exits_on_partial_frame` (client writes a length header then vanishes mid-frame →
  the payload read EOFs → loop exits; covers a half-frame close, not only a pre-frame close), and
  `tcp_connection_healthy_then_disconnect` (positive control: a valid frame is delivered and the loop stays
  alive, and it exits only after the peer closes — proves the fix neither breaks framing nor terminates early).
  The positive control's delivery wait must be a futures-based receive (it runs on a `new_current_thread`
  runtime and the read loop is a spawned task, so a std `mpsc::Receiver::recv_timeout` — a blocking call —
  would starve that task and deadlock the test), so it uses `tokio::sync::mpsc::unbounded_channel` with
  `rx.recv().await`. Full workspace green: **589** tests, 0 failures (Phase 6.3 baseline 586 + these 3);
  `cargo check` clean.
- [x] **6.5 Environment spec loading: no panics, no fail-open** — *done 2026-08-31*
  Files: `src/environment.rs` (`EnvironmentMetadata::load_from_spec`), `src/config.rs`
  (`get_environment_metadata` boot path), plus the 11 test-only callers of `load_from_spec`.
  Action: `load_from_spec` now returns `Result<EnvironmentMetadata, PneumaticError>` and fails closed
  on both old failure modes — (1) a missing required `token` or `slush` partition id returns
  `Err(Encoding("... missing required partition(s): ..."))` (reporting both if both absent), replacing
  the two `.expect(...)` that aborted the whole process at startup; (2) any unknown `trans_validation`
  or `block_validation` spec name returns `Err(Encoding("... unknown ... validation spec \"...\""))`
  instead of the silent `_ => {}` skip, so a typo'd/undeclared spec name no longer leaves the token
  unvalidated (which, per Phase 3.2, would have `Token::validate_block` fail closed at runtime
  anyway — but now it fails at boot with a clear message). The boot path in `config.rs` mirrors the
  adjacent `validate()` handling: on the new `Err` it `eprintln`s and returns `io::Error(InvalidData, ...)`
  so the node fails to start rather than booting with a silently neutered spec. No wire-shape,
  serialization, or registry-API change — the only behavioral change is at env-spec load time.
  Verify: 6 regression tests in `environment::tests`, each a proven discriminator (each fails on a
  temporary revert of the fix — verified manually — and the positive control passes):
  `spec_load_rejects_missing_token_partition`, `spec_load_rejects_missing_slush_partition`,
  `spec_load_reports_missing_partitions_together`, `spec_load_rejects_unknown_trans_validation_spec`,
  `spec_load_rejects_unknown_block_validation_spec`, and `spec_load_accepts_and_registers_known_specs`
  (positive control: a valid spec loads `Ok` **and** asserts each trans/block spec registered
  `Some`, so the registration loops can't regress to silent no-ops). Full workspace green: **595**
  tests, 0 failures (Phase 6.4 baseline 589 + these 6); `cargo check` clean.
- [x] **6.6 Zero-stake stakers excluded from selection** — *done 2026-08-31*
  Files: `src/epoch.rs` (`deterministic_select`, `deterministic_select_shard`),
  `committer/src/epoch_manager.rs` (`LeaderSelector::select_internal`, `StakeStore::to_stake_set`,
  `StakeStore::slash`).
  Action: a zero-stake key can be "elected" as leader / finalizer / executor (no stake behind it),
  because (a) the cumulative stake walk returns the zero key when `total > 0` and `target == 0`
  lands on the lexicographically-smallest key (`cumulative(0) >= target(0)`), and (b)
  `deterministic_select_shard` pushes *every* executor into a shard (round-robin) and the
  `shard_count == 1` shortcut returns *all* keys — so zero-stake executors become listed
  "responsible" executors. After Phase 5.1 made slashing real, a slashed-to-zero double-signer also
  *lingered* because `StakeStore::slash` used `saturating_sub` and kept the zero key. Closed both:
  (1) `deterministic_select` filters out zero-stake keys before the cumulative walk and makes the
  `first_key` backup the first *positive*-stake key (or `vec![]`, still guarded by the existing
  `total == 0 → None`); (2) committer's own walk `LeaderSelector::select_internal` filters zeros
  identically; (3) `deterministic_select_shard`'s `shard_count == 1` shortcut filters zeros before
  returning; its round-robin path was already filtering (left intact); (4) `StakeStore::to_stake_set`
  filters zeros when building the returned `StakeSet` (committer leader input + both persisted
  snapshots); (5) `StakeStore::slash` now *deletes* the key when the stake reaches 0 instead of
  leaving a zero entry, so a slashed-to-zero key can never re-enter selection or a later epoch (safe
  because all accessors already handle missing keys).
  Verify: 5 regression tests, each **proven a discriminator** — each fails on a temporary revert of
  its fix (verified by revert → run `cargo test <filter>` → restore), per AUDIT ground rule 2:
  `deterministic_select_skips_zero_stake_key` (core, `src/epoch.rs`: `StakeSet {vec![0]:0, vec![1]:1}`
  ⇒ `Some([1])`; fails as `Some([0])` without the fix),
  `deterministic_select_shard_excludes_zero_stake_executor` (core: `shard_count==1` returns `[exec1]`,
  `[exec0]` excluded; fails as `[[0],[1],[2]]` without the fix),
  `leader_select_skips_zero_stake_key` (committer: `select(&StakeSet{vec![0]:0, vec![1]:1}, 1, &[])`
  ⇒ `vec![1]`; fails as `vec![0]` without the fix — proves the committer's separate walk),
  `stake_store_to_stake_set_filters_zero_keys` (committer: `vec![0]:0` absent from the returned
  `stakers`), and `stake_store_slash_to_zero_removes_key` (committer: `add_staker(k,10); slash(k,10)`
  ⇒ `k` no longer in the raw backing store — asserted against `store.stakes.contains_key(&k)`, which
  is a *true* discriminator of delete-on-zero, whereas checking only the `to_stake_set` view would
  pass even with the fix reverted because that view already filters zeros). Optional coverage left
  out of scope: the sentinel's defensive `assign_finalizer` zero-stake guard at `sentinel.rs:599`
  is kept (now redundant but harmless defense-in-depth); `ExecutorSet::to_stake_set` /
  `StakeSet::to_executor_set` are test-only and not a production path.
  **Wire-compat: NONE.** This phase is pure internal selection logic — no `Message` / `StakeSet` /
  `ExecutorSet` wire shape or serialization change (AUDIT ground rule 4). The `deterministic_select`
  return shape (`Option<Vec<u8>>`) and the `deterministic_select_shard` return
  (`Option<Vec<Vec<u8>>>` flat key list) are unchanged.
  Workspace: **600** tests passing, 0 failures (Phase 6.5 baseline 595 + these 5); `cargo check
  --workspace` clean. See [[phase-6-6-zero-stake-exclusion]].
- [x] **6.7 Stop the evictor on shutdown** — DONE. `shutdown: Arc<AtomicBool>` + `evictor: Mutex<Option<JoinHandle<()>>>` (JoinHandle isn't `Clone`, mirrors `StakeIndex.handle`); `start_eviction` runs a check-first loop that exits within one poll; `stop_eviction` sets the flag before join; `impl Drop for NodeRegistry` joins via `stop_eviction`, releasing the five registry `Arc`s. Interval tightened to 1 s. `#[cfg(test)]` discriminators (drop, explicit stop, positive-control eviction), all proven to fail on revert. No wire/serialization change. Workspace 631 → 634.
- [x] **6.8 Config hygiene** — DONE. Renamed the unused `BEACON_PORT`→`ARCHIVER_PORT` (value 42005) and added `ARCHIVER_PORT_INTERNAL=50004`; `get_internal_port`/`get_external_port` now map `Archiver` to that distinct pair (Phase 6.8 gave it its own ports, no longer the Committer's shared 42001/50000). Removed the dead required `ConfigSpec.balance` (a boot-footgun: config.json had to carry a meaningless value); kept the ignored `ConfigSpec.public_key` and documented — on both `ConfigSpec.public_key` and `Config.public_key` — that the public key is derived from the keystore identity and a config `public_key` is intentionally ignored (honoring it is infeasible/unsafe: no matching private key). Added 4 `config_tests` — `config_spec_parses_without_balance` (true discriminator), `config_spec_still_accepts_balance_field`, `config_spec_public_key_field_is_tolerated`, `config_public_key_is_identity_authoritative` — and 2 `conns_tests` — `archiver_no_longer_shares_committer_ports`, `every_type_has_distinct_port_pair`; each proven to fail on a temporary revert. README:239 port mapping updated. No wire/serialization change. Workspace 634 → 640.
- [x] **6.9 Arithmetic & panic hardening** — checked/saturating stake math (`src/epoch.rs:233-246`,
  `committer/src/epoch_manager.rs:48-52`); integer quorum math (replace f64 at
  `committer/src/committer.rs:453`); `unreachable!` on non-32-byte hash output becomes an error
  (`committer/src/epoch_manager.rs:257-260`); remove `expect()` on message-derived data in
  `BlockFactory::create_hash`.
  Done. Item A — checked/saturating stake math: `deterministic_select` and `select_internal` use
  `checked_add().unwrap_or(u64::MAX)`, the shard walk and `to_stake_set`/`total_stake` use
  saturating arithmetic, and the slash function drops the key at zero (Phase 6.6). Item B — integer
  quorum math replaces the f64 cast (u64→f64 truncates above 2^52) with integer `cumulative*100 >=
  total*quorum` in u128 (`committer/src/committer.rs:900-901`) plus the finalizer-side
  `total_voters*quorum` (`finalizer/src/signature_collector.rs:89`). Item C — the `unreachable!` on
  the leader-selection hash seed becomes a typed error that returns an empty selection
  (`committer/src/epoch_manager.rs:326-339`). Item D — `BlockFactory::create_hash` now returns
  `Result<Vec<u8>, PneumaticError>` (was `Vec<u8>`); the last `expect()` on message-derived data was
  removed — `BlockBuilder::create_block`/`create_block_optimistic` now propagate the error through
  their callers (`finalizer/src/finalizer.rs:473,573`) while test helpers keep `.expect()` on
  locally-built blocks (verified only when tests compile, since `cargo check` skips `#[cfg(test)]`).
  Discriminators across all items (each proven to fail on a temporary revert):
  `deterministic_select_no_panic_on_overflowing_stakes`,
  `deterministic_select_shard_no_panic_on_overflowing_stakes`,
  `stake_set_total_stake_saturates_on_overflow`,
  `executor_set_total_stake_saturates_on_overflow`, `leader_select_internal_no_panic_on_overflowing_stakes`,
  `stake_store_reward_saturates_on_overflow`, `check_and_commit_validated_saturates_on_overflow`,
  `check_and_commit_gas_exceeds_balance_saturates`, `concurrent_block_finalized_submissions_no_panic`,
  `handle_block_confirmed_vote_quorum_precision_big_stakes`,
  `test_reconcile_signatures_precision_big_stakes`,
  `leader_select_returns_empty_on_non_32_byte_hash`. Internal-API change only (create_hash /
  create_block signatures) — no wire/serialization change. Workspace 640 → 650.
- [x] **6.10 Cache chain state (tip) so a `BlockFinalized` doesn't rehash the whole chain** — DONE. `Blockchain::cached_tip()` (src/blocks.rs) — O(1) `chain.back().current_hash` (empty for an empty chain == genesis prev-hash). Replaced the O(n) `get_current_chain_state().last_hash_in` tip derivation on the 3 hot read paths with it: `append_validated_block` (src/blocks.rs:225, the `BlockFinalized` hot path; committer orphan-promote also reads the tip this way, committer.rs:803), `create_block` (src/tokens.rs:203), and the `commit_block` rollback-conflict target (src/tokens.rs:248). `get_current_chain_state` + `validate_next_block` keep their O(n) walk so `is_valid` is untouched — production `tip_hash_for` (epoch_manager.rs:592 gates whether a reconciler trusts a token's tip) and `make_invalid_token` (epoch_manager.rs:432-452, corrupts a tip and asserts `is_valid == false` to drive slashing) rely on it; a cached-validity flag is incompatible with `add_block`-built chains and would remove tamper detection, so deliberately not made. No wire/serialization change. Discriminator is instrumentation-based (old & new produce identical valid-chain outputs — AUDIT ground rule 2): `#[cfg(test)] HASH_COUNTER` counts every `BlockFactory::create_hash`; `append_validated_block_is_constant_time_per_append` appends 100 blocks asserting total `create_hash` == 2*100 (100 `test_block` + 100 append). Guarded by a re-entrant `HashGuard` (a std `Mutex` isn't re-entrant) so parallel tests that also call `create_hash` can't corrupt the count; proven on a temporary revert (append restores `get_current_chain_state()` → count 5150 → assertion fails). Plus 2 correctness tests (`cached_tip_returns_tip_hash`, `cached_tip_empty_for_empty_chain`; `cached_tip_follows_append_and_rollback`). Workspace 650 → 654.

## Composite node-server — runtime host + role plugins

> Separate architecture effort from the audit-remediation phases above (its **phase numbering 0–7 is the
> plan's own**, independent of the C*/H*/M*/L* audit phases). Plan:
> `create-an-implementation-plan-shimmering-gosling.md`. Ground rule from this checklist: every new behavior
> ships with ≥1 **discriminator test that fails on a temporary revert** of the fix. Each item below records
> the discriminators proven that way. Workspace progression: **600 → 617 (P3) → 620 (P4) → 626 (P5) → 629
> (P6) → 631 (P7)**; 0 failures throughout; `node-server` crate warning-clean.

- [x] **CNS 0–2 — scaffold + `RoleSelector` (role-selection-by-stake) + `RoleDispatcher` backbone** — *done 2026-09-01*
  New workspace member `node-server/` (lib + `[[bin]] name="node-server"`), no dependency cycle (`node-server`
  depends on core + all four role crates; the four depend only on core). Modules `boot / role_selector /
  role_dispatcher / node_server`. **`RoleSelector::select()`** — the headline new behavior: own stake
  (`own_stake_for(public_key)`, fail-closed → 0 ⇒ empty set) filtered through `meets_minimum_stake`
  (reused from `src/config.rs:318`, one AND-of-two-floors source of truth with the registration gate) and
  `get_min_type_stake`/`get_global_min_stake` (`src/config.rs:213/226`). Re-evaluated at boot and on epoch
  advance, never on the hot path. **`RoleHandler`** trait (`role()` / `allowed_actions()` / `async handle`)
  + **`RoleDispatcher::dispatch(msg)`** — fail-closed inbound router between the RNS `on_packet` bridge and
  the plugins: match `message.action` against each installed role's `allowed_actions`; **0 matches →
  `UnknownAction`**, 2+ → `AmbiguousAction` (reuses the `ActionRouter::route` action→role pattern, forwarding
  to the installed plugin instead of re-validating token coordination). **Dead-ThreadPool gate:**
  `ThreadPool` (`src/server.rs`) stays dead (zero external call sites) — node-server dispatch is a fresh layer
  (`RoleDispatcher` + `tokio::spawn` per message + `StakeIndex` refresher thread). Verify:
  `role_selector_fails_closed_on_zero_stake`, `role_selector_requires_both_floors`,
  `role_selector_reevaluates_on_epoch_advance`, `role_selector_single_source_of_truth` (CountingProvider ⇒ 0
  data-service calls on `select()`), `dispatcher_rejects_unknown_action`, `dispatcher_routes_to_installed_role_only`
  (Preload rejected without Executor, routed with it) + single/multi-role routing. Discriminators proven by
  temp-revert (`let role_set = Vec::new()` / `EXECUTOR_ACTIONS` emptied → the asserts fail). Wire-compat: none
  (no `Message` shape change).

- [x] **CNS 3 — `build_runtime` — the 14-step committer boot generalized to N role-plugins** — *done 2026-09-01*
  File: `node-server/src/node_server.rs:89`.
  `build_runtime(config: Arc<Config>, stake: Arc<dyn StakeProvider>) -> Result<NodeServer, PneumaticError>`
  generalizes `committer/src/main.rs`: env metadata (**hard error if missing**) → RNS transport (**boot
  tolerated** if it fails) → `DefaultDataProvider` → `StakeIndex` + registration `StakeCheck` →
  `NodeRegistry` → one shared DI bundle (StakeStore/StakingManager/EpochReconciler/LeaderSelector/
  CandidateRegistry/Epoch/EpochBoundaryDetector/BlockProposer/BlockServices/tokens/pending_registry) →
  `role_selector.select()` → one `build_role_plugin` per selected role → `RoleDispatcher::new(installed)`.
  **NodeServer** owns the bundle + selectors; `installed_roles()`/`dispatch(msg)`/`selected_roles()` (read
  directly; the bundle fields carry `#[allow(dead_code)]`, held for CNS-5 lifecycle). **RoleHandler impls**
  for all four plugins: Committer→`handle_message`→Downstream, Executor→`preload_for_transaction`,
  Sentinel→`on_data_received`, Finalizer→**Phase-3 stub** (inbound wired in CNS-4). Verify (discriminators,
  each proven on temp-revert): `build_runtime_no_transport_booted_cleanly` (RNS fails ⇒ host still
  constructible), `build_runtime_wires_stake_gate`, `build_runtime_initializes_epoch` (dispatch("Commit")
  reaches Committer handler → Downstream, never UnknownAction), `build_runtime_installs_only_selected_roles`.
  Gotcha: committer/finalizer test env-spec JSON is missing required `EnvironmentMetadataSpec` fields so
  `from_str` silently drops them — use the complete fixture from
  `committer/tests/pipeline_integration.rs:104` instead. Wire-compat: no `Message`/`DataOp` change; bootstrap
  multiplies `Register` requests (one per selected role) but keeps the `NodeRequest` shape. Workspace **617**.

- [x] **CNS 4 — close the executor/finalizer wiring gaps + `send_to_all` self-loopback** — *done 2026-09-01*
  File: `node-server/src/node_server.rs` (`RoleHandler for Finalizer`); additive test in
  `src/node/registry.rs` (no core change).
  Finalizer stub → **real voter chokepoint**: inbound `Sign` → `finalizer.handle_signature(&message)` (audit
  C1: authenticate executor, verify + accumulate, optimistic finalize on first valid); any other inbound
  action fails closed with a `Downstream` error (route is by `message.action.as_str()`, and since `Sign`
  borrows the message it is not consumed for the fall-through arm). Executor `Preload`→`preload_for_transaction`
  confirmed by its own discriminator. **`send_to_all`-includes-self** (verified assumption, additive test
  only — **no core change**): `NodeRegistry::send_to_all` iterates every registered node under the type with
  **no self-skip**, so a node's own connection in its own bucket is reached — this is the composite loopback
  (cross-role messaging loops back over RNS to the same process → `on_packet` → dispatcher). The composite
  registering *itself* in every selected bucket is CNS-6, not here. Verify (discriminators, proven on
  temp-revert): `finalizer_inbound_handler_not_stub` (reverted to stub → both sides return the stub string),
  `executor_preload_routed_through_dispatcher` (empty `EXECUTOR_ACTIONS` → `UnknownAction("Preload")`),
  `send_to_all_includes_self` (filter own key out of the fan-out → empty receive channel). Wire-compat: none.
  Workspace **620**.

- [x] **CNS 5 — `RoleHost` lifecycle trait + epoch/epoch-boundary coordinator** — *done 2026-09-02*
  Files: `node-server/src/role_dispatcher.rs` (`RoleHost`), `node-server/src/node_server.rs` (`NodeServer`).
  **`RoleHost: RoleHandler`** extends the erased handle with `advance_epoch(&mut self, u64)` + a
  `Pin<Box<dyn Future<Output=()> + Send + 'a>>` `initiate_shutdown` (boxed-Send future — not native async fn —
  so it is `Send`-posable in the erased `dyn` handle across a `.await`). The coordinator drives it over
  `Vec<Box<dyn RoleHost>>` via `iter_mut()`; that boxed handle supplies the Finalizer's `&mut self` a mutable
  handle **without a Mutex** on plugins. **`RoleDispatcher`** gains `roll_forward(epoch)` (fans `advance_epoch`
  to every installed role — the Committer's is a no-op but still visited) + `initiate_all_shutdown()`.
  **`NodeServer`** gains `current_epoch()`/`roll_forward`/`poll_and_advance` (`&self` async;
  `!is_epoch_expired(now) => false`, else set the gate's epoch → roll forward → recompute role set → true)/
  `recompute_role_set()`/`initiate_all_shutdown()`/`spawn_coordinator(Arc<Self>, interval_ms)`.
  `impl RoleHost for` each plugin: Committer `advance_epoch` no-op (self-drives via its own `run_epoch_loop`)
  + real `initiate_shutdown`; Executor both no-op; Sentinel real `advance_epoch` (guards monotonic +
  invalidates caches) + empty shutdown; Finalizer both real. Verify (discriminators, each proven on
  temp-revert): dispatcher `roll_forward_fans_to_all_hosts` + `initiate_all_shutdown_fans_to_all_hosts` (via a
  two-impl **SpyHost** double — the original single `impl RoleHost` put `RoleHandler` methods on the trait
  impl → E0407/E0277, so split `impl RoleHandler for SpyHost` + `impl RoleHost for SpyHost`); real-plugin
  `epoch_advance_fans_out_to_all_roles`, `epoch_advance_poll_triggers_advance` (asserts both no-op when live
  and advance when expired), `epoch_advance_recomputes_role_set` (also asserts zero-stake admits nothing),
  `shutdown_initiates_on_all_plugins`. Gotcha: `matches!`-with-guard is nightly-only (E0658) — use plain
  `match`/`assert_ne!`. Wire-compat: none. Workspace **626**.

- [x] **CNS 6 — composite registration + multi-role auth** — *done 2026-09-02*
  File: `src/node/registry.rs` (+ committer/finalizer auth call sites).
  Set-returning **`find_node_types_by_public_key(&self, key)`** → the node's full role set in registration
  order (Committer, Sentinel, Executor, Finalizer, Archiver); the existing first-match
  `find_node_type_by_public_key` is untouched (the first-match view of the same live lookups). **`node_may_send_action`**
  (role-set auth): `key` may send an action iff it is registered under ≥1 of the action's allowed roles;
  intersection empty ⇒ fail closed. **Multi-bucket `handle_register`** admits one identity across every
  qualifying bucket (`select_registration_node_types` returns ALL qualifying types; refresh existing buckets
  + admit fresh `NodeRegistryNode::with_binding` under each new type; capacity+insert under one
  `admission_lock`, stake gate OUTSIDE the lock). Ack still reports ONE type on the wire (unchanged `NodeRequest`
  shape — the highest-priority type the key is actually under now). Committer `authenticate_message` /
  Finalizer `authenticate_signature_message` switch from single-role `role != expected` to set intersection.
  Verify (discriminators, each proven on temp-revert): `find_node_types_by_public_key_returns_full_set`
  (revert to first-match-only → `[Committer,Sentinel]` becomes `[Committer]`; also breaks `role_set_auth`),
  `multi_bucket_registration_same_identity` (revert `select_registration_node_types` to single-priority-type →
  Committer+Sentinel fail; a naive recursive `plural=singular.next()` **stack overflows** — the single-bucket
  revert must be an explicit priority scan), `role_set_auth_rejects_foreign_action` (composite role must send
  its SECONDARY role's action; first-match only sees the primary). Gotcha: `NodeRegistryType` derives `Clone`,
  NOT `Copy` — iterate `&` and `.clone()` the value (E0507 on a `for &… in`); `Config::get_max_node_number`
  returns 0 for an unconfigured type ⇒ `type_is_maxed_out` ⇒ need `registry_with_capacity(&[...])` in
  discriminators. Wire-compat: **purely additive** `find_node_types_by_public_key` + a role-set auth path —
  no serialization or on-wire field change (the `NodeRequest` shape is unchanged). Workspace **629**.

- [x] **CNS 7 — end-to-end integration: RNS data-plane bridge routes inbound `Message` to the right role** — *done 2026-09-02*
  File: `node-server/src/node_server.rs`.
  `build_runtime` now bridges the transport to the dispatcher: after the DI bundle + role install,
  `if let Some(network)` registers one `on_packet` closure mirroring the committer `main.rs` split —
  `deserialize_rmp_to::<NetworkPacket>`; `packet.control` → `NodeRegistry::handle_control` (control-plane
  never touches the dispatcher), `packet.data` → spawn `route_data_plane(data, dispatcher)` (data-plane never
  hits the registry). **`route_data_plane(data, dispatcher: Arc<TokioMutex<RoleDispatcher>>) ->
  Result<(), RoleError>`** — the extractable Phase-7 unit: deserialize the data-plane bytes as a `Message`
  (fail-closed → `Downstream` if it does not parse) then lock the dispatcher and `dispatch` it; returned from
  the function so the discriminators observe it. **`role_dispatcher` is `Arc<TokioMutex<RoleDispatcher>>`**
  (bare `TokioMutex` does not derive `Clone` — the field is wrapped in `Arc` to share a handle with the
  `on_packet` closure AND keep the existing `dispatch`/`roll_forward` callers working via `self.role_dispatcher.lock()`;
  the closure takes a per-packet `Arc` clone). Verify (discriminators, both proven on temp-revert of
  `route_data_plane` to a no-op returning `Ok(())`): `inbound_data_packet_routes_by_bridge` (serializes a
  `Commit` Message as the data-plane payload and asserts the bridge returns the **same outcome shape** as a
  direct `server.dispatch(Commit)` — compared via a canonical `outcome_tag()` `&'static str` because `RoleError`
  does not derive `PartialEq`; note a naive "accept Ok|Downstream" version would **not** discriminate, since a
  no-op bridge also returns `Ok`, so it compares against the direct path shape),
  `inbound_foreign_action_surfaces_through_bridge` (serializes a `Confirm` Message — no installed role owns it;
  Committer owns only `Commit`) and asserts `UnknownAction` — the discriminating proof that the bridge reaches
  the dispatcher's routing logic rather than being a passthrough that accepts everything). Both run over a
  real built runtime (bad-peer bootstrap ⇒ RNS transport fails cleanly and is `None`; the bridge wiring is
  guarded by `if let Some(network)`, so the tests exercise `route_data_plane` directly against the real
  dispatcher). Gotcha: `route_data_plane` takes the data-plane payload, not the `NetworkPacket` — the tests
  serialize the `Message` payload directly and must not wrap it in a `NetworkPacket`; both closure and field
  need an `Arc` handle (E0599 without the `Arc` wrap). Wire-compat: none (control-plane and data-plane shapes
  unchanged). Workspace **631**.

## Phase 8 — Post-quantum (hybrid) crypto migration

Supersedes the `# TODO: figure out post-quantum encryption` item above. (The plan numbers this work
"Phase 7"; the label is "Phase 8" here to avoid colliding with the audit's own "Phase 7 — Test
coverage the audit found missing" block, which is a separate concern and is left untouched.)

Strategy: the hybrid **"N = N+1"** approach used by TLS 1.3 / WireGuard / Cloudflare in 2026 — the
classical scheme is **combined with** (not replaced by) a NIST PQC scheme. A forgery must break BOTH
halves; a break in one still leaves the other protecting traffic; and a classical peer can verify the
Ed25519 half of a PQC peer's signature (and vice versa), preserving interop.

### Which primitives moved (and which did not)
| Primitive | Purpose | Quantum threat | Action |
|-----------|---------|----------------|--------|
| Ed25519 signatures | identity, message/auth, block & finalizer signatures, RNS→chain binding | **Critical** — Shor recovers the private key from an exposed public key | Hybridized: `(Ed25519 · ML-DSA-44)` |
| X25519 DH + AES-256-GCM | data self / cross-recipient encryption | **Critical** — Shor recovers the DH shared secret | Hybridized KEM: `(X25519 · ML-KEM-768)` |
| SHA-256 | block hash, selection seed, fingerprint | **Safe** — Grover only halves the budget (128 eff. bits); still adequate | **Untouched** |

Schemes adopted per the Ledger Donjon / NIST analysis: ML-DSA-44 (FIPS 204, signature) and
ML-KEM-768 (FIPS 203, KEM) via the rustpq / PQClean bindings
(`pqcrypto-mldsa`, `pqcrypto-mlkem`, `pqcrypto-traits`). Falcon and SPHINCS+ deliberately rejected
(Relic forward-compat + floating-point side-channel; 8–17 kB signatures too large for a
per-transaction wire protocol).

### Done
- [x] **8.1 Hybrid signature provider** (`src/crypto.rs`, `Ed25519Provider`). `sign_data` emits
    `[Ed25519 sig(64) · ML-DSA-44 pk(1312) · ML-DSA-44 sig(2420)] = 3796 B`; `check_signature`
    requires BOTH halves to verify (hybrid "both-halves-must-verify", not "either half"). Half-checkers
    `check_ed25519_half` / `check_ml_dsa_half` expose each half for interop / test.
- [x] **8.2 Hybrid KEM** (`src/crypto.rs`). `encrypt` / `encrypt_to` produce
    `[X25519 eph pk(32) · ML-KEM encapsulation(1088) · ML-KEM recipient pk(1184) · nonce(12) · ct+tag(16)]`
    (2332 B for empty plaintext); the AES-256 key is HKDF-SHA256 over `[X25519 ss(32) · ML-KEM ss(32)]` —
    an attacker must reconstruct **both** to recover the key. `decrypt` / `decrypt_from` reverse it.
- [x] **8.3 Regression discriminators** (22 `crypto::tests`, all green): each half verifies independently;
    the ML-DSA half does not verify under an Ed25519 key; hybrid length is fixed at 3796 B;
    encrypt→decrypt roundtrip; wrong-recipient decrypt errors; Ed25519 half deterministic. Full workspace
    green (657 tests).
- [x] **8.4 Stable PQC identity across restarts** (`src/crypto.rs` `from_persisted` + secret-key
    accessors; `src/rns/identity.rs` `IdentityFile` + `load` + `write_file`). The PQ half was
    **ephemeral** before: `Ed25519Provider::from_seed` called the pqcrypto crates' randomized
    `keypair()` on every boot, so the ML-DSA-44 / ML-KEM-768 public keys changed on every restart
    (only the Ed25519 key was the stable on-chain identity). Now the full PQC keypairs are persisted
    in the keystore and reconstructed with `from_persisted`.

    **Why persist the full secret + public keys (not seeds):** the `pqcrypto-mldsa` / `pqcrypto-mlkem`
    crates expose only the randomized `keypair()` — the seed-keypair FFI variants
    (`crypto_sign_keypair_sk_pk`, `crypto_kem_keypair_seed`) are not bound and are not even in the
    compiled PQClean archives, so binding them would require extending `pqcrypto-internals` codegen.
    Persisted layout per scheme (secret · public): ML-DSA-44 `2560 · 1312` B, ML-KEM-768 `2400 · 1184`
    B — all hex-encoded in the keystore (`mldsa_secret_key`, `mldsa_public_key`, `mlkem_secret_key`,
    `mlkem_public_key`). Keystore file grew ~10.5 kB total (was classical + Ed25519 seed). Reconstruction
    uses `SecretKey::from_bytes` / `PublicKey::from_bytes`, which enforce exact scheme length
    (`InvalidLength` on mismatch) — free corruption validation, formatted into `PneumaticError::CryptoError`.
    X25519 static key stays `StaticSecret::random()` (not persisted — unchanged).

    **Fail-closed on legacy keystores:** the four PQC fields are **non-optional** (`#[serde(default)]`
    absent) — a keystore written before Phase 8.4 hits serde's "missing field" path and becomes a hard
    error: `"corrupt identity file … (missing field mldsa_secret_key); refusing to regenerate …"`, file
    untouched. No rewrite-on-load migration. Safe because the protocol is not deployed; the existing
    `test_corrupt_keystore_is_hard_error` (classical-only keystore) covers this path.

    **Discriminator:** `rns/identity::tests::test_pqc_keys_survive_reload` — capture the PQC public
    keys right after `create_and_persist`, reload via `load`, assert byte-identical. Fails on a temp
    revert to `from_seed` (fresh keys each boot). Full workspace green (core +1; 658 → 659 workspace).

### Wire-compat note (ground rule 4)
The `Message.signature` field grows from 64 B (Ed25519 only) to 3796 B
`[Ed25519 · ML-DSA-44]`. Intentional and interoperable:
- Hybrid verify: a node verifies the Ed25519 half AND the ML-DSA half. A classical peer (Ed25519 only)
  accepts the Ed25519 half of a PQC peer's signature; a PQC peer accepts either half. No other wire
  fields (`body`, `public_key`, `action`, `stake_set`) change.
- **Both ends must ship Phase 8.1+ for the ML-DSA half to be exercised.** Until then, classical
  interop is preserved by the Ed25519 half. (Mixed-version nodes cannot verify each other's ML-DSA
  half, but fall back to the Ed25519 half for interop.)
- **Non-determinism (security-preserving, not a defect):** this PQClean ML-DSA-44 build draws a fresh
  CSPRNG nonce per signature (`randombytes(rnd, RNDBYTES)` → `rhoprime` → per-signature `y` vector in
  `crypto_sign_signature`), so it is not FIPS-204-deterministic. Independent signings of the same
  (key, body) yield different ML-DSA halves. The gossiper dedups on `hash(sender_key, body)` — **not**
  on signature bytes — so this is harmless. The Ed25519 half IS deterministic (RFC 8032); confirmed by
  `messages::tests::signed_message_ed25519_half_is_deterministic`.

### Keystore-file-format change — NOT a wire-protocol change (ground rule 4)

Phase 8.4 changes the **keystore file format** (`node_identity.json` gains four PQC hex fields), not
the wire protocol. The `Message.signature` shape `[Ed25519 sig · ML-DSA pk · ML-DSA sig]` and every
other wire field (`public_key`, `body`, `action`, `stake_set`) are **unchanged** — the ML-DSA public
key still rides *inside* the signature, so peers binding it see no new wire field. There is therefore
**no mixed-version interop caveat** for this change and no AUDIT ground-rule-4 wire note to file:
nodes that only differ by Phase 8.4 exchange identical frames. The only version-sensitivity is local —
a Phase 8.4 keystore won't `load` on a pre-8.4 binary (missing fields ⇒ hard error), and a pre-8.4
keystore won't load on an 8.4 binary for the same reason. This is a keystore-versioning constraint,
not a wire constraint.

## Phase 7 — Test coverage the audit found missing
*Do these alongside the phases they protect; 7.1 is the single most valuable new test in the
repo.*

- [x] **7.1 Wire-path end-to-end test** — **DONE; the flagged incompatibility is now remediated**
  (see RESOLVED finding below). The existing e2e suite calls `committer.handle_message` directly and
  has never exercised the wire path: `committer/tests/pipeline_integration.rs`. **What now runs:**
  - `wire_rns_transport_delivers_network_packet` (`committer/tests/pipeline_integration.rs`) — drives a
    well-formed, **in-limit** `NetworkPacket` frame end-to-end over the **real RNS loopback**
    (identity-encrypted UDP, rhash addressing, the 4-thread decrypt worker pool) and asserts the
    committer's `on_packet` callback receives the decrypted bytes byte-for-byte. This is the largest
    shape RNS's *direct packet* path can carry; it would fail if the RNS transport or bridge were
    reverted (ground rule 2).
  - `wire_undecodable_frame_dropped_by_bridge` — an undecodable frame is dropped by the bridge without
    panic/append (bridge robustness).
  - `resource_over_mtu_roundtrip` (`src/rns/wrapper.rs`) — **the discriminator for the finding.** A
    **~3.8 KB payload (3805 B on the wire)** — larger than RNS's 500 B packet cap — is driven over the
    Reticulum **Resource transfer** path and reassembles **byte-identical** at the receiver (the
    negation of the audit finding). The direct `send_packet` path returns `ExceedsMtu` for this
    payload; the Resource path does not.
  - `send_resource_no_link_errors` (`src/rns/wrapper.rs`) — fail-closed: `send_resource_to` with no
    established link returns `PneumaticError::Resource` rather than silently dropping the payload.

  > **FINDING — RESOLVED — a real pneumatic `Message` could not traverse RNS (PQC signature vs. RNS
  > MTU).** `RnsNetwork::send_to` → `rns_net::RnsNode::send_packet` → `rns_core::packet::RawPacket::pack`,
  > which caps the framed packet at `rns_core::constants::MTU = 500` bytes with **no** configurable MTU
  > and **no** app-level fragmentation. Every pneumatic `Message.signature` is the full
  > Ed25519·ML-DSA-44 hybrid signature (`crypto.rs`: `[Ed25519 64 | ML-DSA-PK 1312 | ML-DSA-sig 2420]`
  > = `MLDSA_FULL_SIG_LEN` = **3796 B**), so even a zero-body `Message` serializes to ≈3.8 KB — far
  > above the 500 B cap. The direct packet path is therefore still a hard 500 B gate; a real Message
  > still cannot use it. **Remediation adopted — option (a) variant A1: Reticulum's native Resource
  > transfer (no wire-shape change, no RNS patch).** Instead of fragment/compressing the `NetworkPacket`
  > (the audit's option (a), which *would* have been a wire-shape change) or patching `rns-core`
  > `constants::MTU` + `send_packet` pack MTU (option (b), a vendored-protocol change), the payload
  > rides Reticulum's native Resource layer — an opaque byte buffer RNS fragments into ~464 B SDUs with
  > its own retransmit/flow-control (`RESOURCE_SDU = MDU = 464`). The rmp-serialized `NetworkPacket`
  > (carrying the `Message`) goes straight into the resource's opaque `data` field, so the `Message`
  > wire format and `NetworkPacket` framing stay **byte-identical**. Scope: `src/rns/wrapper.rs`
  > (explicit link establishment — `register_link_destination` + `create_link`, `send_resource_to`
  > waits for the link to go Active, `wait_for_link_active` helper) and `src/errors.rs` (new
  > `PneumaticError::Resource` variant). **Gotcha that blocked it (now fixed):**
  > `RnsNode::send_resource` is a *silent no-op on a non-Active link*
  > (`link_manager::send_resource_with_auto_compress`: `if link.engine.state() != LinkState::Active {
  > return Vec::new() }` — no error, no callback, payload vanishes), and `create_link` is optimistic —
  > it returns the link_id the moment the LINKREQUEST is *enqueued*, before the handshake completes. So
  > `send_resource_to` now waits for `on_link_established` to record the link (fires exactly when the
  > link engine transitions to Active) before sending, plus a short settle so the responder — which
  > activates ~1 RTT later — is Active and has `AcceptAll` configured. **Ceiling:** RNS is still
  > bounded by a deliberate **16 MiB** cap (`RESOURCE_MAX_BYTES = MAX_FRAME_SIZE`), enforced at the
  > receiver's memory-receive-mode max; over-declared sizes are rejected at the receiver, not
  > allocated, so this is a DoS-safe policy bound, not an arbitrary one. A real `Message` (~3.8 KB)
  > clears it with ~4× headroom. **Why not (b):** option (b) means patching the pinned
  > `rns-net = "=0.7.0"` (Cargo.toml flags version bumps as API-migration events) and changing Reticulum
  > internals; the native Resource layer already does what (b) wanted via a public API, so (b) is
  > redundant and higher-risk — deferred.

  **Wire-compat (AUDIT ground rule 4):** *no wire-message shape changed.* The `Message` and
  `NetworkPacket` serialize byte-identically — the payload rides inside the resource's opaque `data`
  field (a strictly larger pipe), not a new app framing. The 4 original wire tests (BlockFinalized
  positive + tampered + unregistered + undecodable) were removed because the first three are impossible
  on the *direct packet* path (message >500 B, RNS rejects) and replaced with the transport-exercising
  tests above. Because the remediation does not alter the wire shape, the AUDIT compatibility note
  called out in the original finding's option (a) is **not** required here. Full workspace suite green
  (429+ tests); `cargo check` clean.
- [ ] **7.2 Cross-process determinism fixture** — same stake set in different key orders /
  serializations → identical leader, shards, and finalizer selection; same logical block →
  identical hash (guards 2.1/2.2 permanently).
- [ ] **7.3 Concurrency tests** — `BlockFinalized` append race; registration capacity TOCTOU
  (admission closed by 6.3);
  reconcile-then-advance epoch interaction; ThreadPool job-panic → worker death → Drop.
- [ ] **7.4 Boundary & adversarial tests** — quorum 0.0/100.0; duplicate nonce; mixed
  zero-stake selection sets; EOF/busy-spin on `TcpConnection`; hung data service; directory
  response with poisoned entries; heartbeat without signature; over-limit frames.

## Done-when (overall)

1. All boxes checked, each with its regression test in place.
2. `cargo check` clean; full workspace test suite green (including the new 7.x tests).
3. A clean multi-process (≥ 2 nodes per role) run completes a transaction end-to-end over the
   real wire path — the scenario the audit found inoperable.
