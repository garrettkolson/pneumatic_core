---
id: fact-finalizer-signature-did-not-bind-payload
title: "The finalizer signature was verified against a hash the sender supplied — so it authenticated nothing about the block; and its digest hashed a HashMap, making it unreproducible across processes for any block with 2+ executor votes"
type: fact
namespace: architecture
visibility: namespace
summary: "Found 10/09/2026 while making the committer safe to accept a remote execution result, RESOLVED same day (`294a2bb`). Two defects in one place. (1) `Committer::verify_block_finalizer_sig` called `check_signature(sig, finalizer_addr, finalizer_sig.transaction_hash)` — the hash being verified arrived inside the same message as the bytes it was supposed to authenticate, so the check proved *a hash was signed by a key* and said nothing about the payload. A block with an edited `result_hash` and the original signature pair intact passed. The comment claimed reconstruction was unnecessary because that hash is 'the hash the finalizer actually signed'; that is the wrong half — reconstruction IS the check. Both commit paths used it (`block_authority` on `Commit`, `gossip_authority` on `BlockFinalized`), so every signature gate in the committer rested on it. (2) The digest hashed `serialize_to_bytes_rmp(&signed_tx)`, and `SignedTransaction.executor_sigs` is a `HashMap` — iteration order depends on the process's random `RandomState` seed, so two processes produce different bytes for the same logical block: the digest a Finalizer signed was not reproducible by any other node once a block carried more than one executor signature. Neither surfaced, because nothing re-derived the digest: the first verifier that did would have rejected honest blocks at random, in production. Fixed by one shared implementation in core (`finalizer_digest` + `verify_finalizer_signature`) that the signer also calls, a key-ordered canonical carrier view, and a dedicated `CommitterError::SignaturePayloadMismatch`."
auto_inject: true
applicable_when: "Writing or reviewing any signature verification in this workspace; reasoning about what a block's execution result is bound by; adding a verifier for an existing signer; touching `finalizer_digest`, `verify_finalizer_signature`, `Committer::verify_block_finalizer_sig`, `block_authority`, or `gossip_authority`; building test fixtures that need a valid finalizer signature"
confidence: 0.95
verified_at: "10/09/2026"
verified_by: "dsh-agent"
staleness_signal: "Re-open if any verifier is found that checks a signature against a hash/digest supplied in the same message instead of one it derived from the payload — that is the general form of this bug and it is not confined to this call site. Also re-verify if `SignedTransaction` gains a field that is a HashMap or f64 or anything else whose serialization is not deterministic: the canonical carrier view in `finalizer_digest` must order it, exactly as `canonical_signed_trans_bytes` orders `executor_sigs`. Mutation M12 (verification back to the sender-supplied hash) and M14 (canonical carrier back to a HashMap) both fail their named tests; if either mutation stops failing, the fix has regressed."
tags: [fact, crypto, signatures, committer, finalizer, block-hash, determinism, audit, payload-binding]
edges:
  - target: fact-h12-assumes-lockstep-execution
    type: blocks
    weight: 0.95
    note: "The prerequisite: H12's local compare was the only thing catching a rewritten result, so it could not be narrowed until the signature actually bound the result"
  - target: decision-deterministic-per-tx-routing
    type: depends_on
    weight: 0.85
    note: "The routing decision says WHO may sign; this fact is about whether the signature says anything about WHAT was signed"
  - target: fact-hybrid-signature-not-deterministic
    type: related_to
    weight: 0.7
    note: "Same primitive, opposite failure: there two honest signatures of one message differ; here one honest signature was checked against a message nobody derived"
  - target: fact-composite-per-host-fork-divergence
    type: related_to
    weight: 0.7
    note: "The N-producers fault this became the precondition for fixing"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.85
    note: "Phase 2's commit-path work"
related: []
source_url: "repo:src/transactions.rs (finalizer_digest, verify_finalizer_signature)"
---

# A signature checked against a sender-supplied hash checks nothing

## The shape of the bug

```rust
// committer/src/committer/finalizing.rs — as it stood
let valid = self.identity.ed25519.check_signature(
    &finalizer_sig.signature,
    &block.signed_trans.finalizer_addr,
    &finalizer_sig.transaction_hash,   // ← delivered inside the same message as the payload
)?;
```

Verification has to start from the payload and *derive* the digest. Starting from a hash
that travels alongside the payload proves only that the hash was signed — and an attacker
who edits the payload keeps the `(hash, signature)` pair untouched, so the check passes on
a block that no longer contains what was signed.

The comment that kept it there said the reconstruction was unnecessary because
`transaction_hash` "is the hash the finalizer actually signed". True, and irrelevant: the
question a verifier asks is *is this payload the payload that hash covers*, and only
reconstruction answers it.

What an attacker could change, given `transaction_hash = H(H(canonical carrier) ‖ H(sorted
executor votes))`: the execution result (`result_hash`, `result_data` — the state deltas the
Committer applies), the voter set, the stake fields, the routing record. Everything above
the signature pair is free. Only `create_hash`'s linkage would notice, and that is computed
over the whole block by whoever receives it — recomputing it is not a barrier.

## The second defect, found by a test written to pin the first

The digest was `hash(serialize_to_bytes_rmp(&signed_tx))`, and

```rust
pub executor_sigs: HashMap<Vec<u8>, TransactionSignature>,   // src/transactions.rs
```

`std::collections::HashMap` iteration order depends on the process's `RandomState` seed,
which is randomized per process. So the *bytes* a Finalizer hashed — and therefore the
digest it signed — were not reproducible by any other process for any block with **two or
more** executor signatures.

This is the reason the bug is interesting rather than merely wrong. The fix inverts the
risk: writing the missing verifier would have introduced a node that rejects honest blocks
at random, in production, on a condition (2+ votes) that no unit test had ever exercised.
The digest test written to pin the fix — deliberately computing the expected value from the
specification rather than by calling the function twice — was what found it, on the first
run, with two votes in a map.

`canonical_signed_trans_bytes` (the block hash) had already solved this, collecting the same
map into a `BTreeMap` "mirroring the finalizer's own canonical signature ordering". The
block hash was never affected. The signature digest, written separately, was.

## The fix, and why one implementation

`pneumatic_core::transactions`:

- `finalizer_placeholder_sig(tx)` — the signature slot's pre-signing value, which is part of
  the signed surface.
- `finalizer_digest(signed, hash)` — `H(H(CanonicalFinalizerCarrier) ‖ H(vote bytes ordered
  by voter key))`, where the carrier embeds the transaction whole (`result_hash` **and**
  `result_data`) and orders the votes through a `BTreeMap`.
- `verify_finalizer_signature(signed, expected_key, crypto, hash)` — derive, compare,
  verify, with a typed `FinalizerSignatureFailure` so each caller maps refusals to its own
  meaning.

The **signer calls the same function** (`finalizer/src/block_builder.rs`). Two
implementations of a digest drift, and the quiet drift — a verifier that checks a
sender-supplied digest — verifies perfectly. Both committer paths (`block_authority` under
the key the routing decision names, `gossip_authority` under the block's declared address)
go through one helper, so neither can regress independently.

`CommitterError::SignaturePayloadMismatch` is its own variant: "someone changed this block"
is an incident, "this block was never signed correctly" is a bug, and an operator grepping a
node log needs them apart.

## What the test fixtures revealed

12 committer unit tests and 3 integration tests fabricated signatures — `transaction_hash:
b"pipeline_no_conflict_tx_hash"` and friends — because fabricating was the only way to get
past a verifier that trusted fabrication. They now sign the reconstructed digest. One test
needed more: editing `finalizer_addr` after signing is *itself* a payload change, so the
"verified under the routed key, not the self-reported address" test re-signs after the edit,
which states the claim more strongly than the original did.

The refusal test asserts its own control in the same body: the tampered block is
self-consistent (its `current_hash` is re-derived, so nothing but this check objects), and
the honest block from the same builder must be accepted. A verifier that refused everything
would otherwise pass.

## Generalization

Any verifier that takes its digest, hash, or canonical bytes from the same message as the
payload it authenticates is this bug. The check to run on a new verifier: *does the value
being signed come from the receiver's copy of the data, or from the sender's claim about it?*
If the latter, the check is decorative.
