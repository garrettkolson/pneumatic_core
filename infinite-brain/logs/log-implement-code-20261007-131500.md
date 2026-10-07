---
id: log-implement-code-20261007-131500
type: log
operation: implement-code
date: "2026-10-07T13:15:00"
namespace: pneumatic
summary: "Implemented ADR-019: deterministic selection is now salted with the tip of the chain the transaction actually extends. `DataProvider::latest_block_hash(partition_id)` is deleted rather than renamed (the single string used as both partition and token id was the root cause); `Sentinel::selection_tip`/`chain_tip_of` supply the salt, an unreadable token is an error and never an empty salt, the sentinel's reassignment path carries the same salt through the retry, and the committer's epoch-leader seed now goes through `canonical_chain_tip` (sorted-first token id) instead of DashMap iteration order. Genesis's env-keyed placeholder token is retained but documented as vestigial. Suite 1143 to 1146/37/0; two of the three new tests mutation-verified against the pre-fix behaviour."
affected_nodes: ["decision-selection-salt-per-token-tip", "task-multihost-testnet-rollout", "fact-sharding-exists-unexercised", "fact-self-referential-quorum-denominator", "repo:src/data.rs", "repo:sentinel/src/sentinel/processing.rs", "repo:sentinel/src/sentinel/finalizing.rs", "repo:committer/src/committer/epoching.rs", "repo:data-service/src/genesis.rs"]
tags: ["log", "implement-code", "consensus", "deterministic-selection", "salt", "leader-election", "sharding", "audit-h3"]
---

Roadmap Phase 0 item 1, closed. The interesting part is that the recorded root cause was
wrong until this turn.

The vault said the salt collapsed to empty because `latest_block_hash(environment_id)`
looked up a token named `"env"` that did not exist. Reading `GenesisSpec` showed the
lookup **succeeded**: `seed_partition_token` (default true) writes an empty-chain token
under the environment id precisely so that lookup resolves. So the salt was never failing —
it was reading a placeholder whose chain can never advance, which pins it to the genesis
value **permanently**. That is strictly worse than an intermittent failure, and it is the
exact condition AUDIT H3 exists to prevent. Corrected in the decision node.

Chose deletion over renaming. A renamed accessor still returns "the tip of a chain nobody
named", so the next caller has to invent a chain — which is how this started. With the
method gone, a caller must name a token id and read its chain; the defect is not
expressible. 13 `impl DataProvider` blocks were untouched because they never overrode it.

Two implementation notes worth keeping:

- **The salt read sits inside the existing fallible deterministic attempt at the
  reassignment site**, not before it. My first version put it before the `match`, which
  preempted the candidate-based fallback the code already sanctions for a missing stake
  snapshot; one of the rejection tests caught it by asserting the fallback's error string.
  A fallback candidate is a visibly different outcome. An empty salt is not — it looks
  like genesis. That distinction is why the helper itself stays fail-closed.
- **The committer's leader seed was nondeterministic, not just mis-sourced**:
  `.iter().map(…).next()` over an `Arc<DashMap<Vec<u8>, Token>>` picks whichever token
  hash-shard iteration yields. One token: harmless. Two: two committers can elect different
  leaders, and one committer can change its mind on rehash. `canonical_chain_tip` sorts
  token ids first — the repo's own `C6` rule, missed at this site. Extracted as a free
  function so determinism is testable without building a committer.

Test fallout was instructive rather than annoying. Three existing tests failed because
their fixtures carried stake snapshots and executor sets but no token, and the salt is now
fail-closed. Two of them assert *deterministic* routing, so giving them the token is the
honest fix, not a workaround — and the epoch-follow test got stronger for it: its expected
pick now has to be computed with the transaction's own chain tip, so the test fails if the
handler salts with anything else. The two pre-existing tip-sensitivity tests keep their
assertions but pass the salt explicitly and share an identical provider, so they can no
longer discriminate on a provider's guess.

Mutation-verified two of the three new tests by reverting the fix in place and restoring
byte-identically with `cp`/`diff`: dropping `token_ids.sort()` fails the committer test on
the sorted-first tip; making `selection_tip` swallow the lookup error fails
`selection_tip_refuses_a_missing_token` with its own explanation. Worth noting the
weaker half honestly: with only two tokens in the fixture, the forward/reverse *equality*
assertion may pass by luck under iteration order — the tip assertion is what carries that
test.

Suite: 1143 → **1146/37/0**. Still open, deliberately: recording which salt was used, so a
past committee can be re-derived. That is Phase 0 item 3's job and it touches the
block/transaction surface, so it belongs with the quorum-denominator work rather than
sneaking in here.
