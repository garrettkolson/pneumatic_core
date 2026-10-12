---
id: log-mesh-agreement-20261009-203000
type: log
operation: convert-note
date: "2026-10-09T20:30:00"
namespace: pneumatic
summary: "The four-host mesh converges for the first time — `tips.sh` exit 0, all four hosts at the same tip and sequence (`pneumatic:stage3`, `3076d5d`) — via `Committer::committed_txs`, an epoch-bounded committed-transaction record read before the commit path touches the registry; the gate fired identically on all four hosts against the same rival block; D1 (two Sentinels disagreeing on one transaction) is unfixed and still produces rival blocks, now discarded rather than appended; liveness is the new top blocker at 2 of 8 delivered; suite 1229 → 1230/0/37, M17/M18 caught"
affected_nodes: ["fact-two-decisions-one-transaction-double-commit", "fact-h12-assumes-lockstep-execution", "decision-deterministic-per-tx-routing", "task-multihost-testnet-rollout"]
tags: ["log", "convergence", "committer", "multihost", "routing", "duplicate-commit"]
---

# Agreement, and the gate that earned it

```
committer-1  sequence 2  tip 5fba3f86…     executor-1   sequence 2  tip 5fba3f86…
sentinel-1   sequence 2  tip 5fba3f86…     finalizer-1  sequence 2  tip 5fba3f86…
AGREEMENT: all 4 committers hold the same tip at the same sequence
```

Four rehearsals to get here: hosts refusing each other, then one producer refused by three, then
three hosts agreeing while a fourth pulled ahead, then this.

## The choice that made it short

The user picked the epoch-bounded cache over the durable index, and the interesting part is what
that decision forced into the open. A cache is a half-measure by construction, so the field's doc
comment had to say so where a reader would meet it: **empty after a restart**, so the duplicate
window reopens across a restart; **rollback not reflected, because rollback does not exist yet**
(`Blockchain::remove_block` has no non-test caller; `CommitWinnerAfterRollback` has no rollback
behind it), so whoever lands rollback owns dropping the rolled-back block's transaction or a losing
block becomes a transaction that can never be committed. Pruning by epoch means a duplicate from
the previous epoch escapes. Each of those is a limit named while it is still cheap to fix, rather
than a property someone rediscovers during an incident.

## Placement was the whole design

The check is the first statement in the commit function, before the registry is touched. Not for
performance: the case it exists for is the one where the registry has nothing left to say. A
committed transaction's entry is released and removed, so when the rival block arrives there is no
booked block hash and no held decision. The reasoning that made the fix look hard — "an
admission-side gate can't catch this" — is just the same fact seen from the other side: after
removal the transaction looks new.

## Evidence the failure mode was real

```
[dup] refusing tx=ph2-1098 booked=5fba3f86… (epoch 1) incoming=82ee6b2c…
```

Identical on all four hosts, against the same rival block. That matters more than the AGREEMENT
line: a green rehearsal could have meant the race simply didn't happen this run. It happened, on
every host, and every host refused it.

## What is still broken

`LoserDiscarded` fires three times per host in the same rehearsal. Two Sentinels still disagree
about which Finalizer owns a transaction, and the rival blocks keep coming — the chain now drops
them instead of appending them, which is the difference between a fork and a dropped block, not a
fix to the disagreement. D1 is open, named on the routing decision's residuals.

And convergence turned out to be easier than liveness: **2 of 8 submissions** reach a committed
block. The topology question I have been deferring — two blocks contending for one parent, and who
wins — is no longer the blocker, because the loser is now discarded deterministically; the open
axis is `BlockCommit`/`Registry` refusals dropping six of eight submissions on the floor.

## Verification

Suite 1229 → **1230 / 0 / 37**. M17 (gate removed) and M18 (booking never written) both caught.
The new test drives the rival block through a re-pooled entry and asserts the entry is gone first,
so it cannot pass by the registry refusing for an unrelated reason.
