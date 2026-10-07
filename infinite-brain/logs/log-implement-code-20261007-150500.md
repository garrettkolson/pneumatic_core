---
id: log-implement-code-20261007-150500
type: log
operation: implement-code
date: "2026-10-07T15:05:00"
namespace: pneumatic
summary: "Implemented roadmap Phase 0 item 2: per-key sends now go through a new `NodeRegistry::send_to_peers_blocking` that carries the same delivery-failure accounting as `send_to_all`, returns the undelivered targets, and uses one thread and runtime per batch instead of per key. Making the send report failure exposed a live liveness defect — `handle_rejection` re-read a Finalizing entry through the Validated-only `get_transaction`, so the lookup failed on every rejection, the error was swallowed by `if let Ok(tx)`, and the reassignment send never ran at all. Suite 1146 to 1150/37/0; the delivery test is mutation-verified against the pre-fix shape. Also normalized 17 `relates_to` edges to the canonical `related_to`."
affected_nodes: ["fact-finalizer-reassignment-never-delivered", "task-multihost-testnet-rollout", "decision-selection-salt-per-token-tip", "repo:src/node/registry/fanout.rs", "repo:sentinel/src/transaction_notifier.rs", "repo:sentinel/src/sentinel/finalizing.rs", "repo:src/registry/pending.rs"]
tags: ["log", "implement-code", "delivery-accounting", "sentinel", "finalizer", "liveness", "silent-failure", "vault-hygiene"]
---

Went in for the accounting fix, which was expected to be mechanical. It was mechanical,
and the accounting was the *diagnostic* — the defect it revealed is why the item was
written the way it was.

The reassignment path had been reading the transaction with `get_transaction` on an entry
the handler had just moved to `Finalizing`, and that accessor only serves `Validated`
(`src/registry/pending.rs:246-257`). So the lookup failed on **every** rejection. Not a
race — a state precondition checked against a state the caller itself had just set. The
`if let Ok(tx)` swallowed the certainty, the send was never reached, and the handler
returned `Ok(())`. Then I checked for a safety net and there isn't one:
`request_single_finalizer` has exactly one caller, `request_finalizer` has no production
caller at all, the sentinel has no timeout that revisits a transaction awaiting a finalizer
response, and `PendingTransactionRegistry` has no TTL. So the transaction sits in
`Finalizing` until the process restarts, and the recovery path is what strands it — at the
moment the cluster is already degraded, because a finalizer just rejected work.

Four layers each would have made it visible and all four were missing: the accessor's
documented contract; `if let Ok(..)`, which compiles the same whether or not the call ever
succeeds; the fire-and-forget send, which could not have reported a failure either; and the
tests, which asserted `on_data_received(..).is_ok()` and that the entry named the new
finalizer — **both true while the send was dead**. That last one is the lesson worth
keeping: an assertion about a return value and an assertion about in-process state are both
satisfiable with an empty wire. The new test registers the chosen finalizer with a
recording connection and requires that exactly one payload arrive, deserializing to a
`FinalizerRequest` whose body carries the reassigned transaction's id. Mutating the handler
back to "skip the send, return Ok" fails it with *zero means the send is dead*.

Design point worth keeping: `send_to_peers_blocking` returns the undelivered targets rather
than only counting failures, because a target that is not in the bucket has no rhash the
registry knows, so the `(rhash, node_type)` counter structurally cannot hold it. A counter
keyed by identity cannot represent "this identity does not exist" — the return value is
what closes that hole.

Two things I want on the record about my own process:

- **I wrote a wrong comment and caught it before committing.** My first version of the fix
  explained the failure as the handler holding the entry's write lock. The real reason is
  the Validated-only state precondition, which I confirmed by reading `pending.rs` after
  the test error quoted the exact message. The lock story would have pointed the next
  reader at locking when the thing to fix was a state contract.
- **I wrote a fixture that was silently false.** I registered all six finalizer candidates
  so "whichever one gets picked is present", and it still failed with `NoTarget` —
  `register_peer` returns a `bool` and refuses past `get_max_node_number`, so most of those
  registrations never happened. Now the test computes the expected pick first, registers
  that one peer, and *asserts the registration returned true*. The capacity refusal is still
  silent for production callers, so I left it as a named open item on Phase 0 item 2 rather
  than pretending it is closed.

Also fixed two sibling silences in the same pass: `send_to_all_blocking` scored a peer
evicted between collection and send as a **successful** delivery (`else { Ok(Ok(())) }`),
and the sentinel's reassignment call site no longer discards the send result — it logs and
returns the error, after releasing the entry so the lock is not left behind on the error
path.

Suite: 1146 to **1150/37/0** (+3 registry accounting tests incl. a positive control, +1
delivery test). Vault: new fact node, Phase 0 item 2 marked implemented with its remaining
half named, INDEX at 146. Separately, the graph had 17 edges typed `relates_to`, which is
not in `_system/EDGE-TYPES.md`; normalized them to `related_to`. Four more types in use
(`refines`, `supersedes`, `refined_by`, `precedes`) carry real meaning but are also not in
the canonical list — I did not unilaterally rewrite the schema or flatten them into
`related_to`, because that is a contract decision for the vault owner.
