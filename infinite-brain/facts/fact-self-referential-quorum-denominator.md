---
id: fact-self-referential-quorum-denominator
title: "Per-transaction finalization quorum divides by the stake that showed up, not by the set that was assigned"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/07/2026: `SignatureCollector::reconcile_signatures` (`finalizer/src/signature_collector.rs:161`) computes its quorum denominator as `total_stake = candidates.iter().map(stake).sum()` — the stake of the signatures that actually arrived — and if that threshold is never reached it falls through to `candidates.first()` and proceeds anyway. Nothing anywhere compares the voters to the executors the sentinel assigned (grep for assigned/expected_executors/shard_executors in the collector and `finalizer/finalizing.rs`: no hits). Meanwhile `try_finalize` passes the *global* `(total_stake, total_voters)` from `resolve_stake_metrics()` (`finalizing.rs:48-52,108`) into the block. So each block records a global denominator that was never used for the decision. Benign at `shard_count: 1` only because selection returns every positive-stake executor and delivery is assumed complete."
auto_inject: true
applicable_when: "Any work on quorum, sharding, finalization, committee assignment, fault-tolerance claims, or the security consequences of dropping a message"
confidence: 1.0
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If the collector ever receives the assigned executor set, if `check_quorum` gains a production caller, if reconcile_signatures rejects a shortfall instead of falling through, or if blocks stop recording unenforced voter metrics"
tags: [fact, quorum, finalizer, sharding, consensus, security, fault-tolerance]
edges:
  - target: fact-sharding-exists-unexercised
    type: relates_to
    weight: 0.95
    note: "Why enabling shard_count > 1 does NOT fail closed on a global denominator — the per-tx denominator quietly becomes the shard's own stake"
  - target: fact-observer-stake-paradox
    type: relates_to
    weight: 0.85
    note: "Same lesson one layer down: a quorum is only as strong as the set its denominator names, and here the denominator names whoever arrived"
  - target: fact-mesh-verification-probe
    type: relates_to
    weight: 0.8
    note: "Composes with uncounted per-key sends: a send nobody counts shrinks a denominator nobody declared"
  - target: task-multihost-testnet-rollout
    type: relates_to
    weight: 0.8
    note: "Phase 0's third item and Phase 7's real subject: make the denominator a declared, verifiable set before scoping it to anything"
related: ["[[Transaction sharding is already implemented and unit-tested — but has never run with shard_count > 1 across a network]]"]
source_url: "Empty"
---

# Per-transaction finalization quorum divides by the stake that showed up

Found 10/07/2026 while working out whether enabling `shard_count > 1` would break the
pipeline. The expectation was that a global denominator would make sharding fail closed —
a shard can never produce two-thirds of the *whole* validator set's stake, so the
transaction should stall loudly. It does not stall, and the reason is more interesting
than a sharding bug.

## What the code does

`reconcile_signatures` (`finalizer/src/signature_collector.rs:144-205`) is the step the
finalizer runs to turn executor votes into a block:

```rust
let total_stake: u64 = candidates.iter().map(|(_, s, _)| *s).sum();   // ← only who arrived
if total_stake == 0 { return Ok(ReconciledSignatures { ..empty }); }
…
let reached = (cumulative as u128) * 100 >= (total_stake as u128) * quorum_pct;
…
if winning_finalizer.is_empty() {          // quorum never reached
    winning_finalizer = candidates.first()…;   // proceed anyway
}
```

Three properties follow, all verified:

1. **The denominator is self-referential.** One signature from one nonzero-stake executor
   is 100% of `total_stake`, so quorum is met immediately. The comment says as much: "in
   the single-finalizer model this is always true after the first quorum-crossing
   signature."
2. **A shortfall is not an error.** Falling through to `candidates.first()` means the
   function cannot fail to produce a result. The doc comment even notes "caller's
   `check_quorum` should have prevented this" — but `check_quorum` **has no production
   caller** (only tests and this comment reference it).
3. **Nothing ties votes to assignment.** No comparison exists between the collected voter
   set and the executors the sentinel selected. The assigned set is used to decide *who
   receives the preload*, and then never again.

And the block itself carries the opposite numbers: `try_finalize` resolves
`(total_stake, total_voters)` from the **global** current-epoch stake set
(`finalizer/finalizing.rs:48-52`) and passes them into `build_signed_transaction`
(`:108-115`). So a block attests to voter metrics that governed nothing.

## Why it is benign today, and why "benign" is doing a lot of work

At `shard_count: 1`, `deterministic_select_shard` returns **every positive-stake
executor** (`src/epoch/leader.rs:181-194`, sorted for determinism, zero-stake excluded).
So the assigned set is the whole validator set, every executor receives the preload, all
of them sign, and the self-referential denominator *happens* to equal the global one. The
check is correct **by delivery completeness**, not by construction.

That makes it fragile in exactly the direction the rest of this vault has been cataloguing:

- **It composes with uncounted sends.** `request_single_finalizer` and the shard preload
  path discard send results and bypass `delivery_failures`
  (`fact-mesh-verification-probe`, roadmap Phase 0). A send that nobody counts shrinks a
  denominator nobody declared. Fewer votes arrive → the threshold moves down with them →
  the transaction finalizes "successfully" on a smaller base, with no error and no metric.
- **Zero-stake exclusion is the one real guard.** Selection drops zero-stake executors, so
  a slashed-to-zero node cannot be listed as responsible. That is deliberate (AUDIT Phase
  6.6) and it is the only place in this path where the denominator is chosen rather than
  inherited.

## The contrast: the committer's gate is real

Block *confirmation* does have a declared denominator. `committer/quoruming.rs:55-75`
takes `total_stake` from `stake_set_cache` for that block and compares
`cumulative_stake * 100 >= total * quorum_pct`, with `quorum_percentage` read from
`env_data`. So the protocol has one genuine global gate (committers confirming a block)
and one that is decorative (finalizers assembling a transaction). Any claim about
fault tolerance should name which one it means.

## Two config details, corrected

The composite node passes literal `66.6` and `4` into `Finalizer::new`
(`node-server/src/node_server/plugins.rs:126-127`) while holding `env_data` in the same
call. The accurate reading:

- `66.6` lands in `quorum_percentage`, which **is** used — `reconcile_signatures` reads
  it. So `env_data.quorum_percentage` is genuinely ignored on this path.
- `4` lands in `total_voters`, whose only consumer is `check_quorum` — which nothing in
  production calls. So it is inert, not dangerous. (An earlier reading here conflated the
  two.)

## Why this governs the sharding plan

"Shard-scoped quorum denominators" is the phrase in the roadmap, and this fact is why the
phrase is wrong-shaped. The denominator is not currently global-but-shardable; it is
*undefined*, derived from arrivals. So:

- Enabling `shard_count > 1` **would work**, and would silently redefine per-transaction
  attestation as "the shard's own stake, minus whatever failed to deliver" — no
  declaration, no verification, no bound on the attesting fraction of the validator set.
- The prerequisite for any committee design is to make the denominator a **declared set
  that the block names and the receiver verifies** — the assigned executor set for that
  transaction and epoch. Only then does "scope it to a shard" mean anything.
- Per-transaction security is bounded by the size of the selected committee, which is
  `N / shard_count`. Today nothing enforces a floor on that.
