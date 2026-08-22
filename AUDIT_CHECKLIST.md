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

- [ ] **2.4 `remove_block` pops the tip** (L7)
  File: `src/blocks.rs:127-129`.
  Action: `pop_back()` (tip), not `pop_front()`; update callers/tests.
  Verify: unit test on a multi-block chain.

## Phase 3 — Transaction & block security
*Closes: C3, C5, H12, H15.*

- [ ] **3.1 Bind the authenticated submitter to `tx.sender`** (C3)
  Files: `src/transactions.rs` (add sender-signature field), sentinel validation path.
  Action: require a sender signature over the canonical transaction; verify it; reject when the
  authenticated envelope sender ≠ `transaction.sender`.
  Verify: a peer cannot submit a transfer debiting an account it does not control.

- [ ] **3.2 Real block validation, fail closed** (C5)
  Files: `src/environment.rs:205` (block validator registry created empty),
  `src/tokens.rs:864-869` (accept-all `DefaultBlockValidator`).
  Action: populate the registry in production wiring; remove the accept-all fallback — no
  validator registered = reject the block, don't silently pass.
  Verify: a `BlockFinalized` for a token with no registered validator is rejected.

- [ ] **3.3 Atomic, validated tip append in `handle_block_finalized`** (C5)
  File: `committer/src/committer.rs:344-399`.
  Action: verify hash + linkage + (Phase-1) proposer signature; perform read-tip and append
  under one lock scope (no read-guard-then-`get_mut` gap).
  Verify: concurrent sibling blocks → exactly one appended; **update the double-append test at
  `committer.rs:2041-2047`** which currently asserts the buggy behavior.

- [ ] **3.4 Orphan handling for non-tip blocks** (H15)
  File: `committer/src/committer.rs` (BlockFinalized path).
  Action: buffer blocks whose `previous_hash` isn't the current tip (bounded, TTL'd) and
  re-evaluate on tip advance; or explicitly reject with a re-request signal. No silent drop.
  Verify: out-of-order delivery of N blocks → all eventually committed; partition/rejoin
  scenario test.

- [ ] **3.5 Commit the validated payload, not whatever arrived** (H12)
  File: `committer/src/committer.rs` (commit path), `src/transactions.rs`
  (`Committed.block_hash` currently stores a `token_id`).
  Action: before committing, match the incoming commit's transaction payload against the
  validated/pooled transaction (hash comparison); fix the `block_hash`/`token_id` field
  misnomer.
  Verify: a commit whose payload differs from the validated tx is rejected.

## Phase 4 — Production wiring: make the pipeline actually run
*Closes: H4, H5, H7, H8, M11, M12.*

- [ ] **4.1 Repair `committer/src/main.rs` wiring** (H4)
  Files: `committer/src/main.rs:150, 168, 175, 178, 191, 210`; `committer/src/committer.rs:895`.
  Action: populate `PendingTransactionRegistry` from the live pipeline (commit currently always
  fails `TransactionNotInFinalizing`); load `StakeStore` from the data service at boot; stop
  discarding `propose_blocks` output; fix the `CandidateRegistry` double-wiring/shadowing
  (moved into `EpochReconciler`, then a second instance built for the committer); make
  `sign_binding(...)` a hard boot error, never `unwrap_or_default()`.
  Verify: end-to-end test that boots the real committer (no test-only registry injection) and
  commits a transaction.

- [ ] **4.2 Sentinel routes on the current epoch, not literal 1** (H5)
  File: `sentinel/src/sentinel.rs:221` (and every other literal-`1` routing call site;
  `current_epoch` at `:45` is write-only).
  Action: route via `current_epoch`; keep it updated from epoch-advance events with snapshot
  cache invalidation.
  Verify: after an epoch advance, new transactions route against the new epoch's executor
  set/stake snapshot.

- [ ] **4.3 Harden the data-service channel** (H7, H8, M2, M5)
  Files: `src/data.rs:16-17`, `src/conns/senders.rs:29-45, 60-76`, `src/conns/factories.rs:49-58`,
  `src/conns/listeners.rs:15-21`.
  Action: absolute, 0700-scoped socket path (drop the relative `"data"`); authenticate the peer
  (at minimum unix peer-credential check, prefer shared secret); 4-byte-BE length framing with
  the 16 MB cap (reuse `get_data`); read/write timeouts on every stream; UDS listeners bind a
  per-UID runtime dir and return `Result` instead of `expect()` on bind failure.
  Verify: (a) a hung/slow data service times out and degrades registration only, not the whole
  RNS worker pool; (b) a pre-created socket path fails startup cleanly (no panic, no symlink
  hijack); (c) response > 16 MB rejected.

- [ ] **4.4 Stake gates: per-type, real, off the hot path** (H7, H8)
  Files: `src/config.rs:205-219` (uniform `min_stake: 10`), `src/environment.rs`
  (`CostModel.global_min_stake` never consulted), `src/node/registry.rs:329`
  (stake gate runs on the RNS worker pool).
  Action: differentiate per-type minimum stakes and enforce `global_min_stake`; move the
  blocking data-service stake check off the RNS worker pool (async/off-thread) so a slow data
  service cannot wedge the 4-thread network pool.
  Verify: four concurrent stalled stake checks leave the RNS pool responsive.

- [ ] **4.5 Gas accounting: right partition, no swallowed errors** (M11)
  File: `committer/src/committer.rs:265-273`.
  Action: query `token_partition_id` (not `main_environment_id`); surface `get_user`/`save_user`
  errors (log at minimum; decide protocol semantics for deduction failure).
  Verify: failed deduction is observable and cannot silently free gas or overdraw.

- [ ] **4.6 Atomic keystore write** (M12)
  File: `src/rns/identity.rs:217-252`.
  Action: write temp file + `rename()`; on boot, a corrupt keystore is a clear error with
  recovery guidance (backup restore), never a silent regenerate.
  Verify: kill the process mid-write → boot reports the corruption cleanly.

## Phase 5 — Economics & consensus enforcement
*Closes: H1, H2, H3, H6, H9, H13, H14, H16(self-signed), M10, M13.*

- [ ] **5.1 Make slashing real** (H1)
  Files: `committer/src/committer.rs:685-686` (`StakingOp::Slash(key, 0)`),
  `committer/src/epoch_manager.rs:40-45`, `:173-223` (`reconcile_internal` never emits slashing
  ops).
  Action: slash a configured real amount (default: full stake); make epoch reconciliation
  actually apply slashing ops for resolved conflicts; remove or honor the dead
  `finalization_conflicts` field.
  Verify: double-signing test asserts the offender's stake actually decreases by the configured
  amount.

- [ ] **5.2 Discard losers on conflict; bound the registry** (H2)
  Files: `committer/src/committer.rs:613-711`, `src/epoch.rs:680`
  (`remove_conflicted` has zero production callers).
  Action: on `DiscardLoser`, reject/undo the losing block's append; call `remove_conflicted`
  after resolution; enforce a max size on `CandidateRegistry`; give `misshapen_tokens` a real
  remediation path or delete it.
  Verify: contested (token_id, previous_hash) → exactly one block remains in the chain;
  registry size stays bounded under repeated conflicts.

- [ ] **5.3 Unpredictable selection seeds** (H3)
  Files: `src/epoch.rs:180-183, 225-227, 395-398`.
  Action: seed = `SHA-256(domain ‖ epoch_number ‖ prev_block_hash)` with a distinct domain byte
  per selection type (leader / shard shuffle / finalizer / shard index), per ADR-003.
  Verify: same (epoch, stake set) with different prev_block_hash → different leader/shards.

- [ ] **5.4 One epoch writer; authenticated epoch advance** (H9, M8)
  Files: `committer/src/committer.rs:558-600` (`handle_epoch_reconcile`), `:775-795`
  (`advance_epoch`), `src/environment.rs` (spec-attested snapshots).
  Action: authenticate `EpochReconcile` (Phase-1 envelope auth closes the unauthenticated
  advance); single source of truth for the epoch number — reject/queue a second advance for the
  same epoch, never rewind; persist a hash/attestation with each saved stake snapshot and
  verify on load; surface `save_stake_snapshot`/`save_executor_set` errors (currently
  `let _ =`).
  Verify: reconcile-then-advance does not reuse an epoch number; a corrupted snapshot file is
  detected at load, not trusted.

- [ ] **5.5 Protect token replacement** (H13)
  File: `committer/src/committer.rs:307-314`.
  Action: `TokenDistribution` may not replace an existing token id from an arbitrary peer —
  require the appropriate authenticated role and reject conflicts (or define an explicit,
  authorized overwrite flow).
  Verify: a peer cannot swap in a token (chain/metadata) for an id that already exists.

- [ ] **5.6 Enforce nonces; `amount: None` must not pass** (H14, M12 from blocks/tx audit)
  Files: `src/registry.rs` (pool accepts duplicate `(sender, seq)` — only `seq == 0` is
  checked), `src/validation.rs:180`, `src/action_router.rs:215,229`.
  Action: reject duplicate `(sender, seq)`; require `amount` (or define explicit
  zero-amount/no-transfer semantics) instead of `Option` flowing through every gate.
  Verify: replayed nonce is rejected; `amount: None` is rejected at admission.

- [ ] **5.7 Validate quorum/risk/economic config at spec load; wire the real risk gate** (H6)
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

- [ ] **5.8 Conflict resolution: verified proposer, all candidates** (M10)
  Files: `src/epoch.rs:588-622`, `committer/src/committer.rs:633-661`.
  Action: take proposer identity from the verified leader signature, not the block's
  self-declared `proposer_key`; resolve against **all** candidates (fold), not only
  `candidates[0]`; replace the attacker-grindable equal-stake hash tie-break (or document and
  bound it); implement or delete `TieFlagBoth`.
  Verify: a forged `proposer_key` on an incoming block cannot steer the resolution branch.

- [ ] **5.9 Fix the self-signed token path or delete it** (audit: validation+tokens / sentinel)
  Files: `sentinel/src/sentinel.rs:207-208` (`let _ = tx;` — path swallows the transaction),
  `src/validation.rs:161-171` (owner rule inverted: owner is *banned*; String-vs-bytes encoding
  mismatch), `TokenFactory` (never sets the `"owner"` metadata the check reads).
  Action: either make owner operations actually work (owner set at mint, bytes-consistent
  check, tx delivered) or remove the path and its specs.
  Verify: an owner can execute a self-signed operation end-to-end (or the path no longer
  exists).

## Phase 6 — Infrastructure & reliability hardening
*Closes: remaining Medium/Low items.*

- [ ] **6.1 Timeouts on all blocking I/O** (M from infra audit) — see 4.3 for the data
  channel; also bound per-send time in `src/node/registry.rs:525-595` (`send_to_all` issues
  sequential blocking `get_response` calls inside an `async fn`).
- [ ] **6.2 `send_to_all` observability + concurrency** — log (or count) every failed
  delivery with rhash and type (all sends are `let _ =` today); send concurrently; make
  `send_to_all_blocking` construct its own runtime instead of assuming one
  (`registry.rs:585-594`).
- [ ] **6.3 Atomic registration admission** — capacity check + insert under one lock (close the
  check-then-insert TOCTOU with a blocking stake gap in the middle)
  (`src/node/registry.rs:157-161, 318-348`).
- [ ] **6.4 `TcpConnection` EOF is terminal** — `ReadError => continue` busy-spins at 100 % CPU
  after peer disconnect; `break` on EOF (`src/conns.rs:89-97`).
- [ ] **6.5 Environment spec loading: no panics, no fail-open** — missing Token/Slush partition
  is a boot `Result`, not `.expect()`; unknown validator spec names are logged (or rejected),
  never silently skipped (`src/environment.rs:146-156, 176, 188`).
- [ ] **6.6 Zero-stake stakers excluded from selection** — skip zeros in the stake walk (or
  delete keys that reach 0 in `StakeStore::slash`); filter in `to_stake_set()`
  (`src/epoch.rs:233-246`, `committer/src/epoch_manager.rs:40-45, 65-69`).
- [ ] **6.7 `ThreadPool` fixes** (public API, latent) — drop the receiver mutex guard before
  running the job; `catch_unwind` around jobs with logging + worker respawn; no
  `join().unwrap()` in `Drop` (`src/server.rs:79-96, 116-131`).
- [ ] **6.8 Stop the evictor on shutdown** — `Drop`/`CancellationToken` for the eviction
  thread; it currently leaks for process lifetime with `Arc`s to all five registries
  (`src/node/registry.rs:64-90`).
- [ ] **6.9 Config hygiene** — distinct port pair for Archiver (shares 42001/50000 with
  Committer, `src/conns.rs:18-19, 28-29`); remove or default the dead required `balance`
  field; honor or document the ignored `public_key` config (`src/config.rs:254-276`).
- [ ] **6.10 Arithmetic & panic hardening** — checked/saturating stake math (`src/epoch.rs:233-246`,
  `committer/src/epoch_manager.rs:48-52`); integer quorum math (replace f64 at
  `committer/src/committer.rs:453`); `unreachable!` on non-32-byte hash output becomes an error
  (`committer/src/epoch_manager.rs:257-260`); remove `expect()` on message-derived data in
  `BlockFactory::create_hash`.
- [ ] **6.11 O(n) full-chain rehash per block message** — cache chain state / incremental
  validation so a `BlockFinalized` doesn't rehash the whole chain
  (`src/blocks.rs:131-155`).

## Phase 7 — Test coverage the audit found missing
*Do these alongside the phases they protect; 7.1 is the single most valuable new test in the
repo.*

- [ ] **7.1 Wire-path end-to-end test** — drive a real transaction through the actual gossiper /
  RNS loopback (the existing e2e suite calls `committer.handle_message` directly and has never
  exercised the wire path: `committer/tests/pipeline_integration.rs:389,513-517`).
- [ ] **7.2 Cross-process determinism fixture** — same stake set in different key orders /
  serializations → identical leader, shards, and finalizer selection; same logical block →
  identical hash (guards 2.1/2.2 permanently).
- [ ] **7.3 Concurrency tests** — `BlockFinalized` append race; registration capacity TOCTOU;
  reconcile-then-advance epoch interaction; ThreadPool job-panic → worker death → Drop.
- [ ] **7.4 Boundary & adversarial tests** — quorum 0.0/100.0; duplicate nonce; mixed
  zero-stake selection sets; EOF/busy-spin on `TcpConnection`; hung data service; directory
  response with poisoned entries; heartbeat without signature; over-limit frames.

## Done-when (overall)

1. All boxes checked, each with its regression test in place.
2. `cargo check` clean; full workspace test suite green (including the new 7.x tests).
3. A clean multi-process (≥ 2 nodes per role) run completes a transaction end-to-end over the
   real wire path — the scenario the audit found inoperable.
