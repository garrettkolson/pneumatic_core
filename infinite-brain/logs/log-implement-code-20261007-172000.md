---
id: log-implement-code-20261007-172000
type: log
operation: implement-code
date: "2026-10-07T17:20:00"
namespace: pneumatic
summary: "Implemented roadmap Phase 0 item 3, and the roadmap was pointing at a dead function. `try_finalize` has no caller — the live standard path is `try_finalize_optimistic`, which per ADR-005/ADR-010 waits for no quorum — so `reconcile_signatures`' self-referential denominator was live only on the shielded path. Auditing the gate that *is* load-bearing turned up three silent failures behind the only stake-weighted quorum an ordinary transaction passes: a `BlockQuorumReached` claim was obeyed without recomputation from any registered role; a committer never counted its own vote, so three equal committers topped out at 66.7% against a 67% threshold and nothing could ever reach `Confirmed`; and votes arriving before their stake set were discarded with `Ok`. All three fixed with counters, plus `ResponsibleSet` on the finalizer side, deletion of `check_quorum`/`total_voters`/`shard_quorum_percentage`, and the composite reading `env_data.quorum_percentage` instead of a literal 66.6. Suite 1150 to 1155/37/0."
affected_nodes: ["fact-committer-confirmation-gate-did-not-gate", "fact-self-referential-quorum-denominator", "task-multihost-testnet-rollout", "decision-optimistic-finality", "repo:committer/src/committer/quoruming.rs", "repo:committer/src/committer/finalizing.rs", "repo:committer/src/committer.rs", "repo:finalizer/src/signature_collector.rs", "repo:finalizer/src/finalizer/finalizing.rs", "repo:finalizer/src/finalizer/shielded.rs", "repo:src/environment.rs", "repo:node-server/src/node_server/plugins.rs"]
tags: ["log", "implement-code", "quorum", "committer", "finalizer", "finality", "silent-failure", "security", "dead-code", "config-hygiene", "test-baseline"]
---

Item 3 as written in the roadmap said: *the per-transaction quorum denominator is whoever
showed up — fix it by passing the assigned set into reconciliation and counting a
shortfall.* The arithmetic diagnosis was right. The function it named is not on the path.

## The audit detour that found the real thing

`try_finalize` has **no caller anywhere in the workspace** (verified by grep, not by
inference). The live standard path is `try_finalize_optimistic`, which commits on the first
authenticated executor vote and reconciles no signatures — exactly what ADR-005/ADR-010
decided. So `reconcile_signatures` was live only for shielded transactions, whose own
`check_stake_quorum` gate already used a declared total.

That reframed the item. For an ordinary transaction the only stake-weighted quorum in
existence is the committer's confirmation gate, so I read *that* instead — and found three
defects, each of which reported success:

1. **A claim was the same as a verification.** `handle_block_quorum_reached` re-checked
   nothing ("No further quorum check needed — the broadcaster verified quorum"), and the
   role policy mapped `BlockQuorumReached` to `AllowedSenders::AnyRegistered`. Any
   registered node of any role could mark a block `Confirmed` cluster-wide with one signed
   message. Now the policy is `Exact(Committer)` — a *vote* may come from any role, a
   *conclusion* may not — and the receiver re-runs `local_quorum_reached()` before
   upgrading, refusing and counting claims its own arithmetic does not support. `None`
   (cannot judge yet) is refused separately from `Some(false)`.
2. **A committer's own stake could never vote.** The handler skipped `self.public_key`
   because "we already voted via handle_block_finalized" — which broadcasts but never
   recorded. Self stake was in the denominator and unreachable in the numerator: at equal
   stake and 67%, three committers reach 2/3 = 66.7% (`200 >= 201` is false) and can never
   confirm **anything**, with no log line and no counter. Four happened to work, which is
   why composite-shaped tests never saw it.
3. **The normal message order lost the vote.** A vote whose block's stake set had not
   arrived yet hit `return Ok(())`. Peers gossip votes as they validate, so racing ahead of
   block propagation is the ordinary case. Votes are now buffered per block and replayed
   when the stake set lands, with one log line and a counter per block.

## The finalizer side, done against the decision

`reconcile_signatures(tx_id, &ResponsibleSet)`: the set's total is the denominator; a vote's
weight is what the set declares for that key, never the `current_stake` stamped on the vote;
votes from unassigned keys are excluded **and counted**; a shortfall is an `Err` plus
`quorum_shortfall_count()` rather than the old fallthrough to `candidates.first()`. Per the
decision taken at the start of this item: **reject, count, log** — not count-and-finalize.

`Finalizer::responsible_set()` resolves the set from the epoch stake snapshot and
**refuses at `shard_count > 1`**. A shard is chosen from the per-transaction selection salt,
which the finalizer is never given; substituting the global set there would fail every
sharded transaction while looking like a working quorum check. The honest version is
Phase 7's selection record (epoch, salt, committee), verified by recompute-and-compare.

Dead count-based machinery went with it: `check_quorum` (no production caller, counted
signatures not stake, compared against a construction-time total) and the `total_voters`
parameter are deleted across 7 `Finalizer::new` sites, and the composite now passes
`env_data.quorum_percentage` — it had been passing a literal `66.6` while holding
`env_data` two lines away. `66.6.round()` is 67, so no arithmetic changed; what changed is
that the environment now governs.

`shard_quorum_percentage` is deleted outright, from `EnvironmentMetadata`, the spec, the
deploy config, and every fixture, per the earlier decision. It was validated at boot and
read by nothing, which is the worst kind of knob: it invites an operator to believe a
control exists. No `deny_unknown_fields`, so existing `env.json` files keep loading.

## Tests

Suite **1150 → 1155 passed / 37 ignored / 0 failed** (+5: four committer quorum tests and
five collector tests, minus three `check_quorum` tests and one `shard_quorum_percentage`
validation test).

Mutation-verified, each by reverting the corresponding fix:
- trusting the claim again (`match Some(true)`) → `quorum_claim_is_refused_unless_this_node_computes_quorum_too` fails;
- dropping the self-vote → `block_finalized_counts_our_own_vote_and_replays_early_ones` fails with 2 keys instead of 3;
- dropping the replay hook → the same test fails with no votes at all;
- and `only_a_committer_may_claim_quorum_reached` pins the role mapping itself, so tightening `BlockConfirmed` by accident fails too.

`one_vote_is_no_longer_a_quorum` is the one-liner version of the whole item: one vote out of
a declared three is a third of the stake, not 100% of whoever answered.

## The mistake worth recording

To fix dangling commas in JSON fixtures left by deleting `shard_quorum_percentage`, I ran a
regex over every `.rs` and `.json` file in the tree. It touched **185 files** — Rust tolerates
trailing commas before a closing brace, so most of those edits were invisible damage to
unrelated code, and the diff would have been ~1600 lines of noise.

Recovery, and the part worth keeping: I reconstructed the sweep from HEAD (`pat.sub` on the
HEAD blob) and compared it to the working file. Where they matched, the file held nothing but
sweep damage — **167 files, provably**, with zero files outside that set carrying unexpected
changes, which also confirmed my list of files holding real work was complete. Those 167 went
back to HEAD. In the 20 files carrying real work, commas were restored wherever the only
difference from HEAD was a trailing comma — except where the next line was the key I had
deleted on purpose, which is what had made the comma dangling in the first place. 206 commas
restored, workspace compiles, suite green.

The rule this leaves: a whole-tree regex is not a targeted edit. Fix the N fixtures, not the
tree. And when a mechanical sweep does happen, prove the blast radius against HEAD before
restoring anything — "it compiles" would have shipped 185 files of silent damage.

## Residual, stated plainly

- **Phase 0's first exit test is still unmet.** The salt tests drive `StubDataProvider`; "the
  production provider returns a non-empty salt" remains an assumption. Marked in the roadmap.
- **Role-gate *enforcement* is not directly tested** for `BlockQuorumReached`; the mapping is
  pinned by `only_a_committer_may_claim_quorum_reached`, and enforcement is shared with the
  other `Exact` actions. The committer suite has no signed-message test helper at all — worth
  building when a signature-path change needs one.
- `try_finalize` remains in the tree, unwired, with a doc note that it has no caller. ADR-005
  keeps the quorum machinery as the conflict-resolution path, so deleting it would delete the
  only implementation of a mechanism the design still claims. It should be wired or removed on
  purpose, not swept away as a side effect.
