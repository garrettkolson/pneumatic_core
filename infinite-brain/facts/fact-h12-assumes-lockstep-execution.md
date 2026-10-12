---
id: fact-h12-assumes-lockstep-execution
title: "The commit path's H12 payload compare requires the receiver's local execution to equal the block's — so once ONE finalizer produces the block, the other hosts refuse it and only the producer's chain grows"
type: fact
namespace: architecture
visibility: namespace
summary: "10/09/2026, four-container rehearsal, after ADR-004 was actually applied end to end. Routing worked: one finalizer per transaction, no more N blocks per tx (previously committer-1 appended two blocks with different hashes for one transaction). What replaced the divergence is non-convergence: exactly one host committed (executor-1, sequence 2 for 6 txs) and the other three refused every block with `TransactionPayloadMismatch`. Cause: `committer/src/committer/committing.rs:164` compares `transaction.hash()` — SHA-256 over `canonical_bytes()`, and `CanonicalTransaction` **includes `result_hash`** — against the block's transaction. Each host runs its own executor, so a receiver compares the producer's execution result against what IT executed against ITS local token state. That holds only if every host executes every transaction in the same order against the same state — lockstep. It was invisible before because every host built its own block, and each block matched its own producer's local result by construction; routing removed the cover and exposed the assumption. Open design decision: on receipt of a block produced by the assigned finalizer, a committer should INSTALL the block's execution result (after verifying the routing decision and the finalizer signature) instead of requiring its local execution to agree — a different consistency model from the one the audit phases assumed (H12 was written to stop a payload swap on the wire, and it still does that job against the same host's registry)."
auto_inject: false
applicable_when: "Touching `handle_commit`/`handle_block_finalized` or the H12 hash compare; reasoning about why fewer than all committers commit; changing what `CanonicalTransaction` includes; designing cross-host execution convergence; reading rehearsal output where one host's sequence moves and the others stay at 0"
confidence: 0.9
verified_at: "10/09/2026"
verified_by: "dsh-agent"
staleness_signal: "When a committer that receives a block from the assigned Finalizer installs the block's `result_hash`/`result_data` rather than comparing it to a locally executed copy — then all four hosts' sequences move together and `tips.sh` exits 0. Also re-verify if execution becomes deterministic across hosts by construction (a single executor per transaction with the result gossiped ahead of the block), which would make the compare satisfiable."
tags: [fact, committer, h12, execution, convergence, multihost, composite, finality]
edges:
  - target: fact-composite-per-host-fork-divergence
    type: related_to
    weight: 0.95
    note: "The divergence this replaced: N producers per tx became 1 producer, and the remaining gap is convergence, not multiplicity"
  - target: decision-deterministic-per-tx-routing
    type: depends_on
    weight: 0.9
    note: "One-producer routing is what makes the receiver's local-vs-block result compare load-bearing"
  - target: decision-executor-pure-read-only
    type: related_to
    weight: 0.8
    note: "The executor's pure-read model is why each host re-executes locally at all"
  - target: fact-finalizer-signature-did-not-bind-payload
    type: blocks
    weight: 0.95
    note: "The prerequisite, landed 10/09/2026: the local compare was the only check on a rewritten result, so the signature had to start binding the payload first"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.9
    note: "Phase 2's data-plane exit is now blocked here, not on routing"
related: []
source_url: "repo:committer/src/committer/committing.rs"
---

# H12 compares local execution to a remote block, which assumes lockstep

## What the rehearsal showed, in order

| Build | Result |
|---|---|
| before ADR-004 enforcement (`pneumatic:phase2n`) | 4 txs submitted → sequence 3 → 7 on the committer host; every host committing its own block for each tx |
| routing + finalizer/committer gates (`pneumatic:adr004`) | 1 tx submitted → committer-1 appended **two** blocks with two different hashes; the other hosts committed nothing |
| + decision belongs to the transaction, committer compares against the decision it holds (`pneumatic:adr004c`) | 6 txs → exactly **one** host grows (executor-1, sequence 2), three hosts refuse every block |
| + signature binds the payload and H12 scoped to identity, result adopted from the block (`pneumatic:stage2`) | 8 txs → **three hosts byte-identical** (sequence 3, tip `d86ed741…`); every host booked the same 3 txs; `executor-1` booked 6 appends for those 3 ids and its tip moved off |

`./tips.sh` (added the same day) is what makes that readable: it reads all four
hosts' chains through their own data-service sidecars and compares `(sequence, tip)`.
`up.sh` now publishes every sidecar, not just the committer's — while only one chain
could be read, the fault had no observable signature.

Refusal counts on the three silent hosts, from their own logs:

```text
6  role handler error: Network(TransactionPayloadMismatch   ← H12
4  not in executable state                                  ← optimistic path, benign warn
1  role handler error: Network(Registry
1  role handler error: Network(BlockCommit
```

## The mechanism

`handle_commit` (and the `BlockFinalized` path) refuses a block whose embedded
transaction does not hash equal to the one this node holds:

```rust
if transaction.hash()? != commit.proposed_block.signed_trans.transaction.hash()? {
    return Err(CommitterError::TransactionPayloadMismatch(tx_id.clone()));
}
```

`Transaction::hash()` is SHA-256 over `canonical_bytes()`, and `CanonicalTransaction`
includes **`result_hash`** (`src/transactions.rs:715-728`). So the compare is not
"same transaction" but "same transaction *and same execution result*". Every host runs
an executor, and each host executed the transaction against its own token state at its
own moment — so as soon as the hosts have committed different sets of blocks, their
`result_hash` values differ, and the receiver's own value wins: the incoming block is
a mismatch.

This was always true. It was invisible because before routing every host built its own
block, and a block built from a host's own execution trivially matches that host's
local copy. One producer per transaction — the thing ADR-004 asked for — makes the
compare a cross-host agreement requirement, and there is no such agreement in the
protocol.

## The prerequisite landed first (10/09/2026, `294a2bb`)

Narrowing this compare was not safe as written, and finding out why turned into its own
piece of work: the committer verified the finalizer signature against
`finalizer_sig.transaction_hash` — a hash delivered inside the same message as the payload
it was supposed to authenticate — so it bound nothing, and the H12 compare below was the
*only* thing catching a rewritten execution result. `result_data` is outside the sender's
canonical surface entirely, and only the deploy/upgrade/wasm delta sites re-derive
`hash(result_data)`. So Stage 1 made the signature bind the payload for real; only then can
the compare below be narrowed to identity fields. See
[[fact-finalizer-signature-did-not-bind-payload]].

## Resolved for the lockstep half (10/09/2026, `8e4f86e`)

`Transaction::identity_bytes` / `identity_hash` — the same serialization `hash()` does, with
`result_hash` and `result_data` normalized away — replaced the full-hash compare, and the
commit path adopts the block's result before the delta applies. The three legs of the safety
argument are stated at the call site: identity from this compare, result from the reconstructed
finalizer digest, authority from the routing decision. Measured above: three hosts now hold
identical chains, which this mesh has never done.

What the node still asserts, and what it does not. It is NOT resolved that a receiver can be
indifferent to what a routed Finalizer executes: a Wasm `ContractCall` result is checked only
for self-consistency (`hash(result_data) == result_hash`), so a byzantine routed Finalizer
ships a wrong-but-self-consistent Wasm result and it is applied — attribution catches that,
disagreement does not. `DeployContract`/`UpgradeContract` are exempt because those deltas are a
pure function of the transaction and are re-derived. The confirmation axis
(`FinalityStatus::Confirmed`) is where the disagreement half is supposed to live and currently
gates nothing.

The residual this change exposed is a different defect and now the top blocker: `executor-1`
appended the same transactions twice (6 `COMMIT-OK` lines, 3 distinct ids) and its tip left the
majority. That is fork choice and double-append, not lockstep.

## Why H12 is still right

H12 (AUDIT Phase 3.5) exists because the wire `TransactionCommit` carries its own
`proposed_block`, and nothing verified its embedded transaction was the one this node
validated and pooled: a swapped payload would have been committed. It does that job.
The problem is the *scope* of the comparison — `result_hash` is not part of what the
sender authorized (it is produced downstream), so including it turns an anti-tamper
check into an execution-agreement check the receiver cannot win against a remote
producer.

## The decision this node is asking for

Pick one, explicitly, per commit path:

1. **Install** — accept the block's execution result as authoritative (the routing
   decision and finalizer signature already say who was entitled to produce it), and
   apply it. Cheapest; makes convergence real; means the receiver trusts the producer's
   execution, so the producer's authority has to be exactly the routed key — which is
   what `commit_authority`/`gossip_authority` now prove.
2. **Re-execute at the pinned parent** — the receiver re-runs the transaction against
   the block's parent state rather than its own current state, then the compare is
   satisfiable. Correct and expensive; needs the engine to run at a historical root.
3. **Executors gossip results ahead of blocks** and the block binds a result hash the
   receivers have already agreed to — turns this into a quorum question and re-opens
   the sharding design.

Whichever is chosen, `tips.sh` is the exit test: all four sequences move together and
the tips are equal.
