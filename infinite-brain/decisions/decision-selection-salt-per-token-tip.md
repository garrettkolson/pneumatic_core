---
id: decision-selection-salt-per-token-tip
title: "ADR-019: Salt deterministic selection with the transaction's own chain tip, and remove the ambiguous accessor"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Deterministic selection (executor shard, finalizer assignment, epoch leader) is salted with the tip of the chain the transaction actually extends — the transaction's own token — never an environment-level value. The ambiguous `DataProvider::latest_block_hash(partition_id)`, which used one string as both partition and token id, is deleted rather than renamed, so the conflation cannot be reintroduced. A token that cannot be read is an error, not an empty salt. The committer's leader seed now takes the sorted-first token id instead of DashMap iteration order."
auto_inject: false
applicable_when: "Any change to deterministic selection, the DataProvider trait, epoch leader election, finalizer or shard assignment, genesis seeding, or anything that reads a chain tip"
confidence: 0.95
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If a selection site derives its salt from anything other than the chain the transaction extends; if a chain-tip accessor reappears taking a single partition-or-token string; if a missing token yields an empty salt again; if the committer's leader seed stops using a canonical token rule"
tags: [adr, design-decision, consensus, deterministic-selection, sharding, leader-election, dataprovider, determinism, audit-h3]
edges:
  - target: decision-deterministic-per-tx-routing
    type: refines
    weight: 0.95
    note: "That decision fixes the seed's shape (domain byte, epoch, tx id, prev hash); this one fixes what the prev hash is allowed to be"
  - target: decision-deterministic-leader-election
    type: refines
    weight: 0.9
    note: "Epoch leader election keeps its stake-weighted walk but the seed's tip is now canonical, not whichever token DashMap yielded"
  - target: decision-stake-snapshots-in-dataprovider
    type: depends_on
    weight: 0.85
    note: "The snapshot path stays the source of the responsible set; the chain tip comes from the token record, not a snapshot"
  - target: decision-executor-sharding
    type: related_to
    weight: 0.85
    note: "Shard membership selection is the other consumer of this salt; per-token salting applies whether shard_count is 1 or more"
  - target: fact-self-referential-quorum-denominator
    type: related_to
    weight: 0.9
    note: "The reason predictability is not benign: an attacker-chosen tx id plus a known salt names the responsible set in advance"
  - target: fact-sharding-exists-unexercised
    type: related_to
    weight: 0.8
    note: "Where the defect surfaced; that node records the permanent-placeholder mechanism this decision removes"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.8
    note: "Roadmap Phase 0 item 1 — implemented 10/07/2026; the 'record the salt used' half remains open with item 3"
related: ["[[Transaction sharding is already implemented and unit-tested — but has never run with shard_count > 1 across a network]]", "[[Per-transaction finalization quorum divides by the stake that showed up, not the set that was assigned]]"]
source_url: "Empty"
---

# ADR-019: Salt deterministic selection with the transaction's own chain tip

## Context

`deterministic_select` and `deterministic_select_shard` seed from
`(domain, tx_id, epoch_number, prev_block_hash)`. The last element exists for one reason,
recorded at `src/epoch/leader.rs:10-14`: selection should be knowable only *"once the
previous block is actually mined (not merely from the public epoch number + stake set)."*

Both sentinel call sites obtained it from `latest_block_hash(&environment_id)`. That
accessor declared one `partition_id: &str` and used it as **both** a partition and a token
id (`src/data.rs:301`, removed). Genesis then obliged: `GenesisSpec::seed_partition_token`
(default **true**) wrote an empty-chain token under the environment id specifically so the
lookup would resolve.

The consequence is worse than a failing lookup, and this is the part that was easy to miss.
The lookup **succeeded**. It returned the tip of a placeholder token whose chain never
advances — real transactions extend real tokens. So the salt was not intermittently empty;
it was **permanently the genesis value**, which is precisely the "knowable from the epoch
number alone" failure that H3 was written to prevent. Three readings of the same trait
method coexisted — the default (`Ok(None)`), production (partition-as-token-id), and
`StubDataProvider` (partition-as-partition-key, with a comment claiming it mirrored
production while implementing the other reading) — so tests that stored a token saw a
varying salt while production never did.

## Decision

1. **The salt is the tip of the chain the transaction extends** — its own token.
2. **Delete `DataProvider::latest_block_hash`** rather than rename it.
3. **A missing token is an error, never an empty salt.** An empty salt is legitimate only
   for a genuinely empty chain, i.e. genesis.
4. **The committer's leader seed takes the sorted-first token id**, not iteration order.
5. The genesis placeholder record is still written, but is documented as vestigial.

## Why the transaction's own token, and not an epoch-scoped value

- **It is the only value that exists.** A block-lattice has no environment-level tip;
  a transaction extends one token's chain. An epoch-scoped salt would have to be *some*
  token's tip anyway, which reintroduces the tie-break with a whole-epoch blast radius.
  Choosing per-token dissolves the fiction; epoch-scoping institutionalizes it — and the
  original bug *was* an attempt to instantiate a per-environment tip.
- **Canonical for free.** One token per transaction means honest nodes cannot disagree
  about which chain the salt names. No tie-break rule, no ordering argument.
- **Free at the call site.** The sentinel already loads `get_token(&tx.token_id, …)` for
  validation roughly 85 lines before it needs the salt, so `chain_tip_of(&token)` costs no
  round trip on the paths that hold the token.
- **Re-keys most often**: per block, per token.

Rejected — *epoch-scoped state from the committer*. Consistent by construction and
recoverable after the fact, but fixed for the whole epoch and readable by anything that
can read the data service, so committees become computable for the entire epoch in
advance; and the committer deliberately does not persist chain state
(`committer/epoching.rs:81-84`), so it would need new persisted state plus a new sentinel
read path. Both options re-key; the difference is re-key frequency, not secrecy.

## Why deletion rather than a rename

Renaming `latest_block_hash(partition_id)` to something honest still leaves an accessor
whose *result* has no well-defined meaning in a lattice — the next caller has to invent
which chain they meant, which is how this happened. With the method gone, a caller must
name a token id and read its chain. The failure mode is no longer expressible.

`GenesisSpec::seed_partition_token` is kept because existing `genesis.json` files set it
and boot reporting tracks it; removing it would be a config-format change with no gain.

## The committer's separate defect

`advance_epoch_to` seeded leader election with

```rust
self.tokens.iter().map(…).last_hash_in.next()
```

over `Arc<DashMap<Vec<u8>, Token>>` — whichever token hash-shard iteration yielded first.
One cached token: harmless. Two: two committers with the same token set could elect
different leaders, and one committer could change its mind when the map rehashed. The
codebase already has the counter-rule (`C6`, sort-before-select in `src/epoch/leader.rs`).
Now `canonical_chain_tip()` sorts token ids and takes the first, as a free function so the
determinism property is testable without building a committer.

## Consequences

- **Per-selection I/O**: `selection_tip(token_id)` costs one provider read where the token
  was not already loaded (the reassignment path). Deliberate: the alternative was
  guessing a chain.
- **Where the failure lands.** At the reassignment site the salt read sits *inside* the
  existing fallible deterministic attempt, so an unreadable token takes the same
  candidate-based fallback that a missing stake snapshot already takes. That is not the
  fail-open this ADR rejects: a fallback candidate is a visibly different,
  non-deterministic outcome, whereas an empty salt masquerades as genesis and freezes the
  selection seed for everyone, forever. The helper itself never degrades —
  `selection_tip_refuses_a_missing_token` pins that, and it fails loudly when the helper
  is mutated to swallow the error.
- **Sharding now has a real salt.** With `shard_count > 1`, shard membership varies per
  block per token. It still does **not** reduce links or shorten the broadcast tail; see
  `fact-sharding-exists-unexercised` and Phase 7's committee-shape gate.
- **Still open — record the salt.** A tip is a moving value: if the salt is not recorded in
  the selection output, receivers must recompute it (and can disagree mid-flight), and no
  one can re-derive a past committee. That is the same hole as
  `fact-self-referential-quorum-denominator` viewed from the other side, and it is
  deliberately staged with roadmap Phase 0 item 3 because it adds a field to the block /
  transaction surface, which is wire-adjacent.
- **Genesis convention stays explicit.** Empty salt = empty chain. Nothing swallows an
  error into that value anymore; the previous code had
  `.unwrap_or_default().unwrap_or_default()` at both sites.

## Evidence

- `selection_tip_reads_the_transactions_own_token` — two tokens in one environment get
  different salts, and each equals its own chain's tip.
- `selection_tip_refuses_a_missing_token` — an unreadable token yields a `Registry` error,
  with the assertion message stating why an empty salt would be wrong.
- `canonical_chain_tip_is_independent_of_insertion_order` — forward and reverse insertion
  orders agree; sorted-first wins; empty cache is an explicit empty salt.
- `the_env_placeholder_token_round_trips` and the genesis boot assertion now assert on the
  placeholder's *emptiness*, which is the property that made it useless as a salt source.
- The two pre-existing tip-sensitivity tests (`get_shard_executors_changes_with_mined_tip`,
  its finalizer twin) keep their assertions, but the salt is now an explicit argument and
  their providers are deliberately identical — so they can only discriminate on the salt.
