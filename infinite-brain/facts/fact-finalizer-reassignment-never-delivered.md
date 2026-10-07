---
id: fact-finalizer-reassignment-never-delivered
title: "Finalizer reassignment never delivered the transaction: the re-read could not succeed, and its failure was swallowed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "RESOLVED 10/07/2026. `handle_rejection` transitioned a transaction to Finalizing with its new finalizer key, then re-read it with `get_transaction` — which only serves entries in the **Validated** state (`src/registry/pending.rs:246-257`). At that point the entry is always Finalizing, so the lookup failed on every single rejection, not intermittently. `if let Ok(tx)` discarded the error, the send never ran, and the handler returned `Ok(())`. A rejected transaction was reassigned in local state and delivered to nobody, with nothing logged. No timeout, expiry, or second sender exists, so the transaction stayed in Finalizing for the process lifetime."
auto_inject: true
applicable_when: "Any change to the rejection/reassignment path, to registry state accessors, or when a transaction is observed stuck in Finalizing with a finalizer that has never spoken"
confidence: 0.95
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "Fixed since 10/07/2026 — the transaction is now cloned from the lock scope that already holds it. Re-read if a handler re-reads a registry entry through a state-restricted accessor after moving that entry, or if `if let Ok(..)` reappears around a send that can fail"
tags: [fact, sentinel, finalizer, reassignment, silent-failure, liveness, defect, resolved, audit]
edges:
  - target: pattern-fail-closed
    type: related_to
    weight: 0.9
    note: "Failed open by omission: the error existed and was thrown away, so the recovery path reported success"
  - target: fact-control-plane-silent-drop-paths
    type: related_to
    weight: 0.85
    note: "Same class of invisible loss — four control-plane instances and this consensus-path one all survived because no test asserted that a recipient received anything"
  - target: decision-sentinel-routing-authority
    type: depends_on
    weight: 0.85
    note: "The sentinel is the only node that routes transactions to finalizers, so when its send silently skips, no peer can compensate"
  - target: concept-finalizer-role
    type: related_to
    weight: 0.8
    note: "A finalizer cannot act on a transaction it never receives — the reassignment is inert without the delivery"
  - target: decision-selection-salt-per-token-tip
    type: related_to
    weight: 0.75
    note: "Surfaced by making sends fail-closed: the pre-existing test asserted is_ok() on a path whose send had been dead for its entire history"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.75
    note: "Roadmap Phase 0 item 2 — the uncounted per-key sends that hid it, fixed the same day"
related: ["[[ADR-004: Deterministic per-transaction routing (not epoch-wide leader)]]"]
source_url: "Empty"
---

# Finalizer reassignment never delivered the transaction

## The mechanism

`handle_rejection` reassigns a rejected transaction: pick a new finalizer, move
the registry entry to `Finalizing { finalizer_key: new }`, then send the
transaction to that new finalizer. The third step began:

```rust
if let Ok(tx) = self.registry.get_transaction(&tx_id) {
    let _ = self.transaction_notifier.request_single_finalizer(&tx, new_key.clone(), &self.env_data);
}
```

`get_transaction` is not a general lookup. Its doc line says it plainly — *"Get
an immutable clone of a transaction from the Validated state"* — and its body
returns `Err` for every other variant (`src/registry/pending.rs:251-256`). The
entry this handler reads is in **Finalizing**, because the handler put it there
a few lines earlier. The lookup therefore failed **on every rejection**. This is
not a race, not a lock-ordering fluke, not a rare state: it is a state
precondition evaluated against a state the handler itself just set.

`if let Ok(tx)` turned that certain failure into a no-op, and `let _ =` would
have discarded a send error too — but the send was never reached. The handler
returned `Ok(())`.

## Why it survived

Four independent layers each would have been enough to make it visible, and all
four were absent:

1. **The accessor's contract** — documented, and the caller never consulted it.
   `get_transaction_mut` (used successfully everywhere else in the same function)
   is the one that works on any state.
2. **`if let Ok(..)`** — a pattern that compiles identically whether or not the
   call ever succeeds.
3. **The fire-and-forget send** — `request_single_finalizer` spawned a detached
   thread, discarded the send result, and returned `Ok(())` even when the target
   was not in the registry, so a real delivery failure could not have been
   reported either.
4. **The tests** — the reassignment test asserted `on_data_received(..).is_ok()`
   and that the registry entry named the new finalizer. **Both were true while
   the send was dead.** No test ever asserted that a recipient received bytes.

## Blast radius

Rejection is the only path that reassigns a finalizer, and this was its only
delivery step: `request_single_finalizer` has exactly one caller, and the
broadcast variant `request_finalizer` has **no production caller at all**. The
sentinel also has no timeout that revisits a transaction awaiting a finalizer
response, and `PendingTransactionRegistry` has no TTL or eviction — so a
transaction that entered `Finalizing` with an undeliverable assignment stayed
there for the process lifetime. The recovery path stranded the transactions it
was meant to rescue, and it did so invisibly at the exact moment the cluster was
already degraded (a finalizer had just rejected work).

What *was* observable: nothing. Not a log line, not a counter, not an error, not
a test failure.

## The fix

The transaction is now cloned from the lock scope that already holds the entry —
the only place it can be read — and the send result is surfaced: a reassignment
that reaches nobody returns an error and logs, after releasing the entry so the
lock is not left behind.

Delivery is asserted at the byte level, not at the return-value level:
`handle_rejection_delivers_the_transaction_to_the_new_finalizer` registers the
chosen finalizer with a recording connection and requires that exactly one
payload arrive, deserializing to a `FinalizerRequest` whose body carries the
reassigned transaction's id. Mutating the handler to skip the send and still
return `Ok(())` — the pre-fix shape — fails that test with "zero means the send
is dead".

## The general shape

A state-restricted accessor called on an entry whose state the caller just
changed, with the error handled by a pattern that cannot distinguish success
from failure. The reusable rule: **when a caller has just written a state, it
must read from the handle it wrote through, not through a second lookup** — the
second lookup is where the contract mismatch lives, and the swallow is where the
symptom disappears. Tests have to assert that the *recipient received
something*, because every in-process signal can read green while the wire is
empty.
