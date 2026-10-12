---
id: fact-two-decisions-one-transaction-double-commit
title: "Two Sentinels can route one transaction to two different Finalizers, and a Committer honours whichever decision arrived last — so both blocks get committed and that host's chain leaves the majority's"
type: fact
namespace: architecture
visibility: namespace
summary: "Measured 10/09/2026 on the four-host mesh (`pneumatic:stage2`, `8e4f86e`). The diverging host MOVES between runs (executor-1 in one, committer-1 in the next) and always shows more `COMMIT-OK` lines than distinct transactions: in one run committer-1 committed 4 distinct transactions in 6 blocks — `ph2-1091` inside `d413eb8f…` AND inside `e0d3b338…`, `ph2-1094` inside `1ecd6973…` AND `dcce7eec…`. Chain length 6 vs the majority's 4, different tip. Two independent defects make this possible. (D1, cause) ADR-004 says an arriving routing record is HONOURED, never re-attested — and `Committer::held_decision` is nothing more than the current `tx.selection` on the registry entry, so when a second Sentinel's decision naming a DIFFERENT Finalizer arrives after the first commit removed the entry, the re-created entry carries the new decision and the second Finalizer's block passes the authority gate too. The rule that makes a single decision trustworthy has no rule for a second, conflicting one. (D2, why the chain does not defend itself) Nothing at commit time or append time asks whether this transaction id is already on this token's chain: dedup lived entirely in the in-memory `PendingTransactionRegistry` entry, which is released and removed on commit, and `Blockchain` keeps no transaction index — `add_block`/`append_validated_block` do hash chaining only. Fixing D1 alone removes this trigger; D2 is what makes a wrong-but-agreed block recoverable rather than permanent."
auto_inject: true
applicable_when: "Reasoning about why chains diverge after convergence; touching the committer's admission/`held_decision`/`block_authority` path, the removal of committed registry entries, or `Blockchain` append paths; deciding where a duplicate-commit gate belongs; changing Sentinel selection or anything that makes two Sentinels' views differ"
confidence: 0.9
verified_at: "10/09/2026"
verified_by: "dsh-agent"
staleness_signal: "Re-open if `tips.sh` reports DIVERGENCE while hosts report growing chains, and the diverging host has more `COMMIT-OK` lines than distinct transaction ids — that signature is this defect, whatever else has changed. Verify D1 is closed by confirming a Committer that holds decision A refuses an arriving decision B for the same transaction instead of replacing it. Verify D2 by confirming a second block carrying an already-committed transaction id is refused by the chain rather than appended. Also re-verify if `SelectionRecord::same_decision_as` stops being the comparison used, since it is the only thing that can tell 'the same decision, re-delivered' from 'a conflicting decision'."
tags: [fact, convergence, routing, committer, sentinel, fork-choice, duplicate-commit, determinism]
edges:
  - target: decision-deterministic-per-tx-routing
    type: breaks
    weight: 0.95
    note: "The decision guarantees ONE routed Finalizer per transaction given one view; this is what happens when two views differ and the second one arrives later"
  - target: fact-h12-assumes-lockstep-execution
    type: related_to
    weight: 0.9
    note: "The blocker this replaced; H12 is resolved for the lockstep half and named fork choice as the next one"
  - target: fact-composite-per-host-fork-divergence
    type: related_to
    weight: 0.8
    note: "Same class — one host's chain leaving the majority's — different mechanism"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.9
    note: "Phase 2's current top blocker"
related: []
source_url: "repo:committer/src/committer/routing.rs (held_decision)"
---

# Two decisions, one transaction, two blocks

## The signature to look for

`tips.sh` says DIVERGENCE, and the chains are *growing* — so it is not the old refusal failure.
The check is on the diverging host's log:

```
COMMIT-OK lines: 6      distinct transaction ids: 4
ph2-1091 → block d413eb8f…
ph2-1091 → block e0d3b338…      ← same transaction, two different blocks
ph2-1094 → block 1ecd6973…
ph2-1094 → block dcce7eec…
```

The host moves between runs, which is what rules out a role-specific cause and points at
delivery order.

## D1 — the cause: an arriving decision replaces a held one

`Committer::held_decision(tx)` returns `tx.selection` from the registry entry — whatever was
stamped there by the most recent admission. Nothing else holds a routing decision.

ADR-004's rule is that an arriving record is **honoured, never re-attested**, because the
Sentinel that admitted the transaction is the authority on which Finalizer it picked. That rule
is sound for the first record. It says nothing about the second: a transaction admitted by two
Sentinels that disagree carries two records, each honoured, each naming a different Finalizer.

The mesh's composite topology makes the second arrival likely: every host runs a Sentinel, the
same transaction reaches several of them, and ADR-004's candidate set is derived from a node's
local view (registry contents, own addressability, epoch snapshot) — a view that can differ.

The first commit then releases and removes the entry. When the transaction arrives again on the
other path, the re-created entry carries *that* decision, and the other Finalizer's block passes
`block_authority`: sender matches the named Finalizer, the signature reconstructs, the identity
compare matches. Every gate is satisfied because the gates are asked about the wrong transaction
— "was this block built by the Finalizer this node's current decision names", when the question
needed is "was it built by the Finalizer this transaction was routed to".

The corroborating line is in the composite host's own log: the Finalizer role reports
`optimistic finalize replay for tx ph2-1091 … not in executable state for finalization` twice —
the same transaction came through the pipeline more than once, on different paths.

## D2 — why the chain does not refuse it

Nothing asks "is this transaction already committed?".

- `Blockchain::add_block` / `append_validated_block` verify hash chaining, nothing else. There
  is no transaction index on the chain (no tx-id set, no receipt map).
- The only place that knew a transaction had been committed was the `PendingTransactionRegistry`
  entry, and committing **removes** it — that is the documented lifecycle, and correct for
  memory, but it means deduplication had the lifetime of a pointer.
- `wire_authoritative` (the branch that lets an already-`Committed` entry accept a wire block
  again) compares against the *booked block hash*, so it correctly refuses a different block —
  but it only gets to ask that question while the entry still exists.

So the chain has no memory of what it contains. A committed transaction is a fact about the
chain that no node can look up.

## The two fixes, and why they are separate

**D1 (admission):** a Committer that holds decision A must refuse an arriving decision B for the
same transaction — `SelectionRecord::same_decision_as` already distinguishes "same decision
re-delivered" from "conflicting decision". The commit path already does exactly this; the
admission path, which is where the value gets written, does not.

**D2 (chain):** a duplicate-commit gate needs durable state — a committed-transaction index
maintained across append, rollback, and reload, which the chain does not have. Rollback is what
makes it non-trivial: a transaction whose block was rolled back must become committable again,
or the fork-choice rule silently turns a losing block into a permanently uncommitted
transaction. Fixing D1 removes this trigger; D2 is what stops a future D1 from being permanent.
