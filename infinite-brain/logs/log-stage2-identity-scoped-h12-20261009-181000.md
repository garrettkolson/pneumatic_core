---
id: log-stage2-identity-scoped-h12-20261009-181000
type: log
operation: convert-note
date: "2026-10-09T18:10:00"
namespace: pneumatic
summary: "Stage 2 landed (`8e4f86e`): the commit path's H12 compare is now `Transaction::identity_hash` — the full serialization with `result_hash`/`result_data` normalized away — and the committer adopts the routed block's execution result before applying deltas; measured on the four-host mesh for the first time three hosts hold byte-identical chains (sequence 3, tip `d86ed741…`) where before one grew and three sat at 0; new top blocker is that `executor-1` double-appended the same 3 transactions (6 `COMMIT-OK`, tip off the majority), which is fork choice rather than lockstep; `fact-h12-assumes-lockstep-execution` records its lockstep half as resolved with the Wasm self-consistency hole named; suite 1227 → 1229/0/37"
affected_nodes: ["fact-h12-assumes-lockstep-execution", "fact-finalizer-signature-did-not-bind-payload", "decision-optimistic-finality", "task-multihost-testnet-rollout"]
tags: ["log", "committer", "convergence", "h12", "optimistic-finality", "multihost", "testing"]
---

# Three hosts, one chain — and the defect that replaced the old one

## What was decided, and by whom

The user's constraint set the whole shape of this: optimistic finality stays. That ruled out
the design where a block binds a quorum-agreed execution result, and left the smaller claim —
under ADR-005 the block the routed Finalizer produced *is* the answer, so the receiver adopts
its result. Stage 1 (the signature binding the payload) existed only to make that safe to
write.

## The change

`identity_bytes` is `hash()`'s own serialization with the two execution outputs zeroed, not a
new field-by-field view. The reason is the failure mode of view structs: a field added next
month silently leaves the identity compare. A clone-based normalization keeps every field
inside the compare until someone deliberately zeroes it.

Then the commit path copies the block's `result_hash`/`result_data` into the transaction it
carries forward. That is not cosmetic: `apply_deploy_delta`, `apply_storage_delta` and
`apply_upgrade_delta` all read `transaction.result_hash`, so without the adoption the node
applies its own stale result while appending someone else's block.

## The measurement

```
committer-1  sequence 3  tip d86ed741…
sentinel-1   sequence 3  tip d86ed741…
finalizer-1  sequence 3  tip d86ed741…
executor-1   sequence 6  tip bc52f763…
```

Every host booked the same three transactions. Three hold identical chains. Across the
rehearsal series — before, `adr004` (one committer, two different blocks for one tx),
`adr004c` (one producer, three hosts refusing everything), `stage2` — this is the first
agreement the mesh has shown, and it is the exit test the H12 fact named.

The fourth host is the interesting part. `executor-1` logged 6 `COMMIT-OK` lines for 3
distinct transaction ids: it appended the same transactions twice and its tip left the
majority. The lockstep blocker is gone and what remains is fork choice — two blocks contend
for one parent on one host, and the host keeps both. Naming it matters because the previous
three rehearsals all failed the same way (a host refusing), so a green-looking "hosts grow"
result could have been read as convergence.

## What the node refuses to round off

A Wasm `ContractCall` result is checked only for self-consistency — `hash(result_data) ==
result_hash` — not re-executed. So a routed Finalizer behaving badly can ship a
wrong-but-self-consistent Wasm result and it will be applied on every honest host.
`DeployContract`/`UpgradeContract` are exempt because those deltas are pure functions of the
transaction and get re-derived. The text at the call site says plainly that attribution
catches this and disagreement does not, and that Stage 3 — making `FinalityStatus::Confirmed`
gate something — is where it is paid for. Writing the comfortable version ("the signature
authenticates the result") would have been the dishonest one.

## A test that lied

The first version of `a_commits_execution_result_may_differ_from_what_this_node_computed`
asserted the registry entry held the adopted result. The commit path releases and removes a
committed entry, so the test failed from the moment it was written. Then it sat there
reporting CAUGHT for two mutations that it would have failed regardless. The full-suite run
disagreeing with the filtered run is what surfaced it.

Two things follow. A mutation verdict is only as good as the test's ability to pass, so a
mutant run must be preceded by a green unmutated run of the same filter. And the honest fix
was to assert on something durable — the chain tip — with the install itself proven
downstream, where a wrong value has a consequence: the deploy test makes the local and wire
results disagree on purpose and requires the token *the block describes* to exist, so it fails
both if the compare stays strict and if the adoption is removed.

## Verification

Suite 1227 → **1229 passed / 0 failed / 37 ignored**. M15 (adoption removed) caught by the
deploy test and not by the tip test, which only observes acceptance; M16 (compare reverted to
the full hash) caught by both. Reported as such rather than as a clean sweep.
