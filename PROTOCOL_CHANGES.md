Here's a phased roadmap, ordered so each phase is buildable and testable on top of the last. I'm anchoring it to the actual modules/functions I found in the repo so it's concrete rather than abstract.

## Phase 0 — Lock down the design decisions (no code yet)

These choices will ripple through everything below, so they're worth deciding explicitly before touching code:

- **What exactly triggers "a conflict"?** The natural definition given your `Block` struct is: two different valid `Block`s that both reference the same `previous_hash` for the same token (i.e., two proposers building on the same parent). Right now nothing in the codebase can even represent this state — worth writing it down as the formal invariant you're detecting.
- **Does the mandatory 2/3 executor quorum in `SignatureCollector::check_quorum` go away for standard tokens, or shrink to a minimal-validity check?** Full optimistic finality means most tokens shouldn't need any quorum at all in the happy path — that quorum requirement is currently the biggest thing blocking "instant by default."
- **Is voting weight for conflict resolution the same global `StakeSet` used for epoch leader election** (`committer/src/epoch_manager.rs`), or a logically separate "representative" set? Nano keeps these distinct (delegated voting weight vs. block production); you currently only have one pool. Worth deciding on purpose rather than by default.
- **What happens to a losing block/proposer?** Just discarded, or slashed via the existing (currently unwired) `StakingOp::Slash`? Real forks are usually evidence of either a race condition or bad-faith double-proposing — worth punishing the latter.

## Phase 1 — Data model: represent "competing candidates," not just "the chain"

`Blockchain.chain: VecDeque<Block>` assumes one canonical linear history — there's nowhere to put a second, competing block for the same slot. Add:

- A `CandidateRegistry` (DashMap-backed, matching your existing style in `registry.rs`) keyed by `(token_id, previous_hash)` → `Vec<Block>` (or `Vec<(Block, proposer_key)>`), holding not-yet-final proposals.
- A `finality_status` concept per block — `Optimistic` vs `Confirmed` — so downstream consumers (Archiver, wallets, whatever reads chain state) know whether a block could still be superseded.
- Extend `Block` (or the `SignedTransaction` it wraps) to carry the proposer's public key explicitly if it isn't already recoverable from `signed_trans`, since conflict resolution needs to look up stake per proposer.

## Phase 2 — Replace the conflict-detection logic

`EpochReconciler::reconcile_internal()` in `committer/src/epoch_manager.rs` currently compares block hashes at matching indices **across different tokens** — that never validly matches and isn't the fork case you care about. Replace it with same-chain detection:

- On receiving/appending a candidate block for a token, check the `CandidateRegistry` for existing entries at the same `(token_id, previous_hash)` key. If one exists with a different `current_hash`, that's a conflict — build the existing `Conflict { block_a, block_b, stake_a, stake_b }` struct from it.
- This needs to run **at ingestion time in the Committer**, not just at epoch boundaries — epoch-boundary-only detection can't deliver "instant" finality, since blocks would sit unconfirmed for a whole epoch before anyone checks. Epoch-boundary reconciliation can stay as a slower-path safety net for anything missed (e.g., a node that was offline).
- Fill in `stake_a`/`stake_b` for real by querying `StakeStore::get_stake()` for each proposer instead of the current hardcoded `0`.

## Phase 3 — Wire `resolve_block_conflict` into the commit path

This function already exists and already works — it's just never called outside its unit tests. Once Phase 2 produces real `Conflict` data:

- On detection, call `resolve_block_conflict()` with the real stakes.
- Commit the winning block to the token's actual `Blockchain`; drop the losing candidate from the `CandidateRegistry`.
- If you decided in Phase 0 to slash double-proposers, emit a `StakingOp::Slash` for whichever proposer(s) get discarded — but only when the same proposer signed both competing blocks (an honest node relaying a race isn't the culprit).
- Broadcast the resolution outcome via the existing `gossiper`/`MessageDispatcher` machinery so all nodes converge, not just the Committer that happened to detect it first.

## Phase 4 — Make the default path actually optimistic

This is the biggest behavioral change and the one most in tension with the current code:

- For standard (non-self-signed) tokens, replace "wait for 2/3 executor quorum before finalizing" with something like: one Executor executes, one Finalizer signs and dispatches immediately, Committer commits it as `Optimistic`. No blocking supermajority vote in the common path.
- The quorum/voting machinery in `SignatureCollector` doesn't disappear — it gets repurposed as the *conflict-resolution* voting mechanism from Phase 3, invoked only when the `CandidateRegistry` shows a genuine fork, rather than gating every single transaction.
- Decide what "confirmed" means for a client-facing guarantee — e.g., "final after N seconds with no observed conflict" — and expose that via `finality_status`.

## Phase 5 — Networking additions

- Add a vote/dispute message type in `messages.rs` so nodes can broadcast "I saw this candidate block for this slot" and "I vote for block X in this conflict," reusing the dedup/fan-out patterns already in `gossiper.rs`.
- Add conflict-vote aggregation, structurally similar to `SignatureCollector` but scoped to conflicts rather than per-transaction quorum.

## Phase 6 — Testing

- Unit tests simulating two proposers submitting valid, differently-signed blocks against the same `previous_hash` for one token — verify the `CandidateRegistry` catches it and `resolve_block_conflict` picks correctly by stake, then by hash tie-break.
- Concurrency tests (you already do this well elsewhere, e.g. `registry.rs`'s `std::thread::spawn` + `Arc`-shared DashMap pattern) for near-simultaneous candidate submission.
- An end-to-end pipeline test: submit → optimistic commit → no conflict → confirmed, alongside submit → optimistic commit → conflict injected → resolved → slashing applied (if you built that).

## Phase 7 — Docs

Given the project already tracks work in `TASKS.md` with C#-reference-style entries, I'd add a new section there (`Consensus Rearchitecture`) mirroring the existing Phase 1–7 production roadmap format, so this doesn't get lost alongside the sentinel/executor/finalizer/committer completion work already tracked.

---

A natural place to start, if you want to keep it incremental and testable: **Phase 1 + 2** together (candidate registry + real fork detection) can be built and unit-tested in isolation without touching the finalizer's quorum behavior at all — that gives you a correct detector before you change what "instant" means for the happy path in Phase 4, which is the riskier change to get right.
