---
id: fact-committer-confirmation-gate-did-not-gate
title: "The one stake-weighted quorum an ordinary transaction passes did not actually gate: claims were obeyed, self-votes never counted, early votes dropped"
type: fact
namespace: pneumatic
visibility: namespace
summary: "RESOLVED 10/07/2026. Under ADR-005/ADR-010 an ordinary transaction commits optimistically on one executor vote, and the stake-weighted quorum that upgrades it to `Confirmed` runs at the Committer. That gate had three independent holes. (1) `handle_block_quorum_reached` re-verified nothing — \"No further quorum check needed, the broadcaster verified quorum\" — and the role policy let ANY registered node of ANY role be the broadcaster, so one signed message marked a block final cluster-wide. (2) A committer never recorded its own vote, though the code comment claimed it did, so self stake was in the denominator and could never be in the numerator: with equal stake, 3 committers each saw 2 of 3 = 66.7% against a 67% threshold and NOTHING could ever become Confirmed. (3) A vote arriving before its block's stake set was dropped with `return Ok(())` — and because peers gossip votes as they validate, early is the normal case."
auto_inject: true
applicable_when: "Any change to committer confirmation, quorum, finality status, the message role policy, or when reasoning about what a client may treat as final"
confidence: 0.95
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If `handle_block_quorum_reached` stops calling `local_quorum_reached` before upgrading finality; if `BlockQuorumReached` returns to `AllowedSenders::AnyRegistered`; if `handle_block_finalized` stops recording its own vote or replaying `pending_confirmation_votes`"
tags: [fact, committer, quorum, finality, security, silent-failure, defect, resolved, consensus]
edges:
  - target: decision-optimistic-finality
    type: depends_on
    weight: 0.95
    note: "ADR-005/010 make committer stake quorum the only quorum an ordinary transaction ever passes — which is why these three holes matter more than any finalizer-side arithmetic"
  - target: fact-self-referential-quorum-denominator
    type: related_to
    weight: 0.9
    note: "That node found the self-referential denominator in the finalizer; this one is the gate that actually governs, and it failed differently"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.85
    note: "All three were failures that reported success — a trusted claim, an uncounted own vote, a dropped vote with Ok"
  - target: fact-control-plane-silent-drop-paths
    type: related_to
    weight: 0.75
    note: "Same shape: a dropped message that produced no metric, no log, and no error"
  - target: fact-finalizer-reassignment-never-delivered
    type: related_to
    weight: 0.7
    note: "Found the same day by the same route — making a discarded result observable"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.8
    note: "Roadmap Phase 0 item 3; the 3-committer consequence belongs in any cluster-sizing note"
related: ["[[ADR-005/010: Optimistic commit with quorum as dispute mechanism]]", "[[Per-transaction finalization quorum divides by the stake that showed up, not by the set that was assigned]]"]
source_url: "Empty"
---

# The committer's confirmation gate did not gate

ADR-005/ADR-010 decided that standard tokens commit optimistically on a single
executor's signature and a single finalizer's signature, and that the 2/3 stake
quorum is the **dispute and confirmation** mechanism, run by Committers: each node
that validates a `BlockFinalized` broadcasts a `BlockConfirmed` vote, committers
accumulate stake per unique voter against the block's declared stake-set total, and
at quorum someone broadcasts `BlockQuorumReached`, which upgrades the block from
`Optimistic` to `Confirmed`.

So for an ordinary transaction the committer quorum is the only stake-weighted check
in existence. It had three holes.

## 1. A claim was the same as a verification

```rust
// All nodes that receive this transition the block to Confirmed.
// No further quorum check needed — the broadcaster verified quorum.
```

Combined with the role policy — `"BlockConfirmed" | "BlockQuorumReached" =>
AllowedSenders::AnyRegistered` — this meant **any registered node with any role**
(an executor, an archiver, a sentinel) could send `BlockQuorumReached` for a block
hash and every committer would flip it to `Confirmed`. The envelope signature was
verified and the key had to be registered, so this was not unauthenticated; it was
worse in a way, because the trust was *documented* as intentional ("honest broadcasts
from any registered node") while the message carried a conclusion only a committer is
able to reach.

Fixed on both sides: the role gate now requires `Exact(Committer)` — a *vote* may come
from any role, a *conclusion* may not — and the receiver re-runs its own
`local_quorum_reached()` before upgrading. A claim that does not match local
arithmetic is refused, counted (`rejected_quorum_claim_count()`), and logged with a
`SECURITY:` prefix. `None` (no stake set or no votes recorded yet) is distinguished
from `Some(false)` and is also refused: "we cannot judge" is not permission.

## 2. A committer's own stake could never vote

`handle_block_confirmed_vote` began with:

```rust
// Skip our own vote (we already voted via handle_block_finalized)
if voter_key == self.public_key { return Ok(()); }
```

`handle_block_finalized` broadcasts a vote; it never *recorded* one. So the comment
described a state that did not exist, and the consequence is arithmetic: the
denominator is the full stake-set total, self included, while the numerator could
never include self. At equal stake and a 67% threshold:

| committers | best achievable | quorum at 67%? |
|---|---|---|
| 2 | 1/2 = 50.0% | no |
| **3** | **2/3 = 66.7%** — `200 >= 201` | **no, forever** |
| 4 | 3/4 = 75.0% | yes |

Three committers could never confirm anything. No counter, no log line, no error: the
blocks simply stayed `Optimistic` indefinitely. A cluster sized at three — the kind a
"minimum viable" deployment picks — would have looked healthy right up until a client
asked whether a transaction was final.

Fixed by recording the vote in `handle_block_finalized` (which is also where the stake
set is known), with key dedup in the accumulator so a composite identity holding every
role still casts one vote.

## 3. The normal message order lost the vote

A vote whose block's stake set had not been cached yet hit `return Ok(())`. Peers
gossip their vote the moment they validate a block, so racing ahead of the block's own
propagation is the *normal* order, not the pathological one. A committer could lose
every vote it ever received and conclude that nothing had quorum.

Fixed by buffering per block (`pending_confirmation_votes`), replaying when the stake
set lands, and counting the event (`buffered_confirmation_vote_count()`), with one log
line per block rather than one per vote.

## What "fixed" means here

The gate now refuses rather than proceeding: a claim without local quorum does not
upgrade; a shortfall is counted and logged. Per the Phase 0 decision, the trade accepted
is liveness-with-honesty over liveness-by-silence — a block that cannot prove quorum
stays `Optimistic` and *looks* stuck, instead of quietly becoming final on a
denominator nobody declared.

The `shard_count > 1` case is explicitly **refused** in the finalizer rather than
approximated: a shard's responsible set depends on the per-transaction selection salt,
which the finalizer is never given. Using the global set there would fail every sharded
transaction while looking like a working check — which is why Phase 7's "declare and
verify the responsible set" carries a selection record (epoch, salt, committee) instead
of leaving receivers to recompute a value that moves.

## Evidence

`committer/src/committer/tests/quorum.rs`:
`quorum_claim_is_refused_unless_this_node_computes_quorum_too` (mutation-verified:
trusting the claim again fails it), `block_finalized_counts_our_own_vote_and_replays_early_ones`
(mutation-verified both ways: dropping the self-vote gives 2 keys instead of 3, dropping
the replay leaves no votes at all), `a_vote_before_its_stake_set_is_buffered_then_replayed`,
and `only_a_committer_may_claim_quorum_reached`, which pins the role mapping itself.
