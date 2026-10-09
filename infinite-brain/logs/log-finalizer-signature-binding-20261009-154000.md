---
id: log-finalizer-signature-binding-20261009-154000
type: log
operation: convert-note
date: "2026-10-09T15:40:00"
namespace: pneumatic
summary: "Stage 1 of the convergence work landed (`294a2bb`): the finalizer signature is now verified against a digest reconstructed from the received block instead of a hash the sender supplied, via one shared core implementation the signer also calls; found and fixed a second defect in the same digest (a HashMap whose per-process iteration order made the digest unreproducible across nodes for 2+ executor votes); new fact `fact-finalizer-signature-did-not-bind-payload`; `fact-h12-assumes-lockstep-execution` records its prerequisite as landed; Phase 2 direction fixed as optimistic-finality-preserving install, with the confirmation axis named as the next gap; suite 1225 → 1227/0/37, mutations M12/M13/M14 all caught"
affected_nodes: ["fact-finalizer-signature-did-not-bind-payload", "fact-h12-assumes-lockstep-execution", "decision-deterministic-per-tx-routing", "decision-optimistic-finality", "task-multihost-testnet-rollout"]
tags: ["log", "crypto", "signatures", "committer", "finalizer", "determinism", "audit"]
---

# The signature that checked nothing, and the verifier that found it

Three things happened in one sitting, in an order worth recording because the order is the
point.

## The direction question, settled by a constraint

The user picked the byzantine-resistant option — executors gossip results, the block binds a
quorum-agreed value. Four research agents inventoried the machinery, and it turned out the
design would have reversed ADR-005: a block needing a quorum of agreeing results cannot be
produced by the first valid executor signature, which is what optimistic finality *is*. The
user's response — keep optimistic finality — moved the byzantine-resistance requirement onto
the confirmation axis, where ADR-005 had already placed it, and left the commit path doing
the smaller thing: accept what the routed Finalizer produced.

That is the shape now recorded on `fact-h12-assumes-lockstep-execution`: identity from the
local compare, result from the signature, authority from the routing decision — three legs,
three pieces of code.

## The prerequisite, which was not optional

Narrowing the H12 compare was Stage 2 on the plan. It became Stage 2 in fact, because the
one thing currently catching a rewritten execution result is that very compare, and the
signature that was supposed to be doing the work verified a hash delivered inside the same
message as the payload it was supposed to authenticate.

Two details from the investigation deserve to stay visible.

The comment was the reason it survived: *"its `transaction_hash` is the hash the finalizer
actually signed, so the Ed25519 verification needs no reconstruction."* True statement, wrong
conclusion — reconstruction is the check, not an optimization of it.

And the fix inverted the risk. Writing the missing verifier would, on its own, have created a
node that rejects honest blocks at random in production — because the digest it recomputes
hashed `serialize_to_bytes_rmp(&signed_tx)`, and `executor_sigs` is a `HashMap`, whose
iteration order comes from the process's `RandomState` seed. Any block with two or more
executor votes had a digest no other process could reproduce. Nobody noticed because nobody
recomputed. The block hash was unaffected — `canonical_signed_trans_bytes` already collects
that map into a `BTreeMap`, and the signature digest, written separately, did not.

The test that found it is the reason to write digest tests against a specification rather
than against a function: the signer and verifier now share one implementation, so comparing
them with each other proves nothing. It computed the expected digest from the doc comment,
longhand, with two votes in a map — and failed on the first run.

## What the fixtures said

12 committer unit tests and 3 integration tests fabricated their finalizer signatures.
That was forced: fabrication was the only way past a verifier that trusted fabrication.
Fixing them was a honest accounting exercise, and one test needed a real redesign — editing
`finalizer_addr` to test "verified under the routed key, not the self-reported address" is
itself a payload change now, so the fixture re-signs after the edit. The redesigned test
states the claim more strongly.

## Named, not fixed

`FinalityStatus::Confirmed` is set when a block reaches quorum (`quoruming.rs:225`) and read
by nothing but tests. The axis ADR-005 relies on for byzantine resistance currently records
without enforcing — no consumer refuses to treat an Optimistic block as final. That is the
next piece of work after the install, and it is where the honest price of optimistic finality
gets paid or refunded.

Also still standing: nothing re-verifies the executor signatures embedded in a block at
commit time, and `block_builder` overwrites each embedded executor vote's `transaction_hash`
with the signature bytes — so those votes are not re-verifiable from a block as it is
shaped today.

## Verification

Suite 1225 → **1227 passed / 0 failed / 37 ignored**. Three mutations, all caught: M12
(verification back to the sender-supplied hash → the swap test fails), M13 (digest covers the
real signature instead of the placeholder), M14 (canonical carrier back to a `HashMap` →
ordering property fails, i.e. the reproducibility bug stays fixed).

Recorded honestly: the mutation script keyed its backup dict by path, and both M14 edits
targeted `src/transactions.rs`, so the restore re-applied the first edit. Caught because the
suite stopped compiling; repaired. The mutation results above were collected with both edits
live, so they stand — but the script needs a per-edit restore before it is trusted again.
