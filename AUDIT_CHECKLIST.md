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
- [ ] **6.10 O(n) full-chain rehash per block message** — cache chain state / incremental
  validation so a `BlockFinalized` doesn't rehash the whole chain
  (`src/blocks.rs:131-155`).

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

# TODO: figure out post-quantum encryption

## Phase 7 — Test coverage the audit found missing
*Do these alongside the phases they protect; 7.1 is the single most valuable new test in the
repo.*

- [ ] **7.1 Wire-path end-to-end test** — drive a real transaction through the actual gossiper /
  RNS loopback (the existing e2e suite calls `committer.handle_message` directly and has never
  exercised the wire path: `committer/tests/pipeline_integration.rs:389,513-517`).
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
