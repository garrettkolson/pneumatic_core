---
id: fact-self-referential-quorum-denominator
title: "Per-transaction finalization quorum divides by the stake that showed up, not by the set that was assigned"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/07/2026, RESOLVED same day: `SignatureCollector::reconcile_signatures` computed its quorum denominator as the stake of the signatures that actually arrived, and a shortfall fell through to `candidates.first()` and proceeded anyway. Now it takes a declared `ResponsibleSet`, prices each vote by that set rather than by the vote's own stamp, excludes and counts votes from unassigned keys, and returns `Err` on a shortfall. Two corrections to this node's original reading: `try_finalize` (the quorum-gated finalization path it described) **has no caller** — the live standard path is `try_finalize_optimistic`, which by ADR-005/ADR-010 waits for no quorum at all, so `reconcile_signatures` was live only on the shielded path; and the committer-side gate it credited with a real denominator turned out not to verify anything (`fact-committer-confirmation-gate-did-not-gate`)."
auto_inject: true
applicable_when: "Any work on quorum, sharding, finalization, committee assignment, fault-tolerance claims, or the security consequences of dropping a message"
confidence: 0.9
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If `reconcile_signatures` is ever called with a set derived from the arriving signatures instead of a resolved epoch/committee set; if the shortfall `Err` is converted back into a fallthrough; if `try_finalize` gains a caller without being re-audited; or if blocks stop recording unenforced voter metrics"
tags: [fact, quorum, finalizer, sharding, consensus, security, fault-tolerance]
edges:
  - target: fact-committer-confirmation-gate-did-not-gate
    type: related_to
    weight: 0.95
    note: "The gate this node credited as real did not verify its own claims; that is where an ordinary transaction's quorum actually runs"
  - target: decision-optimistic-finality
    type: depends_on
    weight: 0.9
    note: "Why the finalizer-side quorum this node analysed is not on the standard path at all: standard tokens commit optimistically and reconcile nothing"
  - target: fact-sharding-exists-unexercised
    type: related_to
    weight: 0.95
    note: "Why enabling shard_count > 1 does NOT fail closed on a global denominator — the per-tx denominator quietly becomes the shard's own stake"
  - target: fact-observer-stake-paradox
    type: related_to
    weight: 0.85
    note: "Same lesson one layer down: a quorum is only as strong as the set its denominator names, and here the denominator names whoever arrived"
  - target: fact-mesh-verification-probe
    type: related_to
    weight: 0.8
    note: "Composes with uncounted per-key sends: a send nobody counts shrinks a denominator nobody declared"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.8
    note: "Phase 0's third item and Phase 7's real subject: make the denominator a declared, verifiable set before scoping it to anything"
related: ["[[Transaction sharding is already implemented and unit-tested — but has never run with shard_count > 1 across a network]]"]
source_url: "Empty"
---

# Per-transaction finalization quorum divides by the stake that showed up

> **Resolved 10/07/2026.** `reconcile_signatures` now takes a `ResponsibleSet` and its
> total is the denominator; a shortfall is an `Err` and a counter, not a fallthrough;
> votes from outside the set are excluded and counted; a vote's weight is the set's
> declared stake for that key rather than the `current_stake` the vote carries.
> `Finalizer::responsible_set()` resolves it from the epoch stake snapshot, and
> **refuses** at `shard_count > 1` rather than substituting the global set — the shard
> depends on the per-transaction selection salt, which the finalizer is never given, so
> guessing there would fail every sharded transaction while looking like a working check.
> Dead count-based machinery went with it: `check_quorum` and the `total_voters`
> constructor parameter are deleted, and the composite now passes
> `env_data.quorum_percentage` instead of a literal `66.6`. `shard_quorum_percentage` is
> deleted outright — a knob that was validated at boot and read by nothing.
>
> Two things this node got wrong, both worth keeping visible:
>
> 1. **It analysed a dead function.** `try_finalize` has no caller anywhere in the
>    workspace (verified by grep). The live standard path is `try_finalize_optimistic`,
>    which per ADR-005/ADR-010 commits on the first authenticated executor vote and
>    reconciles no signatures — so the self-referential denominator was live only on the
>    **shielded** path, whose own `check_stake_quorum` gate was already using a declared
>    total. Roadmap Phase 0's item 3 was written against the dead path and was right about
>    the arithmetic for the wrong reason.
> 2. **It credited the committer's gate with being real.** The denominator there was
>    declared, yes — and the gate accepted a *claim* of quorum without recomputing it,
>    never counted its own vote, and discarded votes that arrived early. See
>    `fact-committer-confirmation-gate-did-not-gate`.

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

*(See the resolution note above: the paragraph that follows describes `try_finalize`,
which has no caller.)* And the block itself carries the opposite numbers: `try_finalize` resolves
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

## The contrast: the committer's gate had a declared denominator

Block *confirmation* does have a declared denominator. `committer/quoruming.rs:55-75`
takes `total_stake` from `stake_set_cache` for that block and compares
`cumulative_stake * 100 >= total * quorum_pct`, with `quorum_percentage` read from
`env_data`. So the denominator there was declared rather than derived — which is the half of the
problem this node was about. The other half, found while fixing it, is that declaring a
denominator is not the same as checking a numerator: see
`fact-committer-confirmation-gate-did-not-gate`.

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
