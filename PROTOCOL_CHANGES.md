Here's a phased roadmap, ordered so each phase is buildable and testable on top of the last. I'm anchoring it to the actual modules/functions I found in the repo so it's concrete rather than abstract.

## Phase 0 — Lock down the design decisions (no code yet)

These choices will ripple through everything below, so they're worth deciding explicitly before touching code:

- **What exactly triggers "a conflict"?** The natural definition given your `Block` struct is: two different valid `Block`s that both reference the same `previous_hash` for the same token (i.e., two proposers building on the same parent). Right now nothing in the codebase can even represent this state — worth writing it down as the formal invariant you're detecting.
- **Does the mandatory 2/3 executor quorum in `SignatureCollector::check_quorum` go away for standard tokens, or shrink to a minimal-validity check?** Full optimistic finality means most tokens shouldn't need any quorum at all in the happy path — that quorum requirement is currently the biggest thing blocking "instant by default."
- **Is voting weight for conflict resolution the same global `StakeSet` used for epoch leader election** (`committer/src/epoch_manager.rs`), or a logically separate "representative" set? Nano keeps these distinct (delegated voting weight vs. block production); you currently only have one pool. Worth deciding on purpose rather than by default.
- **What happens to a losing block/proposer?** Just discarded, or slashed via the existing (currently unwired) `StakingOp::Slash`? Real forks are usually evidence of either a race condition or bad-faith double-proposing — worth punishing the latter.

**Resolutions (all four are implemented):**

| Decision | Resolution | Evidence |
|---|---|---|
| Conflict definition | Two valid `Block`s, same `(token_id, previous_hash)` | `CandidateRegistry` keyed by `(token_id, previous_hash)` in `src/epoch.rs` |
| 2/3 quorum | Replaced by optimistic commit — first executor sig = immediate finalize | `Finalizer::try_finalize_optimistic()` in `finalizer/src/finalizer.rs` |
| Voting weight | Same `StakeSet` used for leader election and conflict resolution | `resolve_block_conflict()` reads from `StakeSet` in `src/epoch.rs` |
| Losing proposer | Discarded; slashed only when same proposer double-signed | `resolve_block_conflict()` → `ConflictResolution::SameProposerSlash` |

## Phase 1 — Data model: represent "competing candidates," not just "the chain"

**Status: Complete.**

`CandidateRegistry` (DashMap-backed, keyed by `(token_id, previous_hash)` → `Vec<(Block, proposer_key)>`), `FinalityStatus` enum (`Optimistic` / `Confirmed`), and `SignedTransaction.proposer_key` all exist in `src/epoch.rs` and `src/transactions.rs`.

## Phase 2 — Replace the conflict-detection logic

**Status: Complete.**

`EpochReconciler::reconcile_internal()` in `committer/src/epoch_manager.rs` checks the `CandidateRegistry` for 2+ candidates at the same `(token_id, previous_hash)`, resolves real stakes via `StakeStore`, and builds `Conflict` structs. Tested in `epoch_manager.rs` tests.

## Phase 3 — Wire `resolve_block_conflict` into the commit path

**Status: Implemented but not wired into the optimistic path.**

`resolve_block_conflict()` is fully implemented in `src/epoch.rs` with all three outcomes (`DiscardLoser`, `SameProposerSlash`, `TieFlagBoth`), and fully tested. The optimistic commit path does not call it — which is intentional:

- The happy path (no conflict) should be as fast as possible; the optimistic path achieves this by bypassing quorum entirely.
- Conflict detection at the finalizer would add latency to the hot path.
- Epoch-boundary reconciliation (`EpochReconciler::reconcile`) already detects and resolves conflicts as a slower-path safety net.

Wiring conflict detection into the optimistic path itself (before block dispatch) is a future optimization but not a blocker.

## Phase 4 — Make the default path actually optimistic

**Status: Complete.**

`Finalizer::handle_signature` calls `try_finalize_optimistic` on the first executor signature. The quorum path (`try_finalize`) still exists for when subsequent signatures accumulate and quorum is reached.

## Phase 5 — Block awareness gossip (formerly "Networking additions")

**Status: Complete.**

The original plan assumed all executors see every transaction, making distributed vote/dispute protocol necessary for conflict awareness. With executor sharding (only one shard processes each tx) and epoch rotation (executors reshuffle each epoch), conflicts are so rare that a full vote protocol is unnecessary.

Instead, a simple block announcement gossip was implemented:

- **`BlockConfirmed` message** in `finalizer/src/message_dispatcher.rs` — serializes `Block` and broadcasts to both Committers and Archivars after optimistic commit
- **`handle_block_confirmed` in Committer** (`committer/src/committer.rs`) — validates chain linkage (non-fatal if behind), validates block hash (fatal if tampered), appends to blockchain, propagates to archivars
- **Wired into `try_finalize_optimistic`** — called after `send_to_committers`
- **5 tests** — 4 committer (valid append, orphan ignored, tampered rejected, unknown token error) + 1 dispatcher serialization
- **No vote/aggregation protocol** — conflict detection is local to each node's `CandidateRegistry`, resolution uses the existing `StakeSet` via `resolve_block_conflict` at epoch boundaries

This is a notification, not a consensus mechanism. Nodes still converge independently at epoch reconciliation if they missed a block announcement.

## Phase 6 — Testing

**Revised scope:** The original plan included distributed conflict vote tests and near-simultaneous competing block tests. With sharding, the conflict scenario is much narrower:

- **Unit tests:** `CandidateRegistry` concurrent inserts (done), `resolve_block_conflict` tie-breaking (done), optimistic commit path (done via existing finalizer tests)
- **Happy path e2e:** optimistic commit succeeds, finality status = `Optimistic`, then confirmed after quorum or epoch boundary
- **Block awareness:** `BlockConfirmed` gossip arrives, local chain state advances (Phase 5 implemented, 5 tests passing)
- **Epoch-boundary conflict:** reconciler detects competing candidates at chain tip, applies `DiscardLoser` or `SameProposerSlash` (existing tests in `epoch_manager.rs` cover the happy/conflict cases)
- **Not needed:** distributed vote/dispute tests (no protocol), near-simultaneous competing block tests at the network level (sharding makes this effectively impossible — only one shard per tx)

## Phase 7 — Docs

**Status: Complete.**

TASKS.md updated with executor sharding + optimistic commit phases. README.md updated with ADR-009 (Executor Sharding) and ADR-010 (Optimistic Commit). This document updated to reflect revised Phase 5/6 scope with block gossip completion.

---

## Summary: what changed and why

The original Phase 5/6 assumed a flat executor registry where every node processes every transaction, making distributed vote/dispute protocol necessary for conflict resolution. Executor sharding + optimistic commit changes the conflict landscape fundamentally:

1. **Each tx goes to only one shard** — a proposer can only win in their shard, not globally.
2. **Epoch rotation reshuffles executors** — stable cartels can't form.
3. **Optimistic commit doesn't wait for quorum** — conflicts that escape the finalizer are resolved at epoch boundaries, not during transaction processing.

The conflict resolution machinery (`CandidateRegistry`, `resolve_block_conflict`, `StakingOp::Slash`) exists and is tested. It operates as a slower-path safety net, not as a real-time consensus protocol. The block announcement gossip is now implemented, letting other nodes learn about committed blocks without polling an archiver.
