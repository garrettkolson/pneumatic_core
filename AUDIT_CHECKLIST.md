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
