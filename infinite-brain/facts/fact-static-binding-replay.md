---
id: fact-static-binding-replay
title: "A signature over a static payload is a permanent credential — audit every repeatable proof"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/05/2026: `binding_payload` signs only `(rhash, requested_type, requester_types)` (`identity.rs:510-517`) — no nonce, no timestamp, no counterparty. For a one-time `Register` that is harmless; for the *repeatable, expensive* directory query on the same payload it meant one observation of a legitimate query was a permanent subscription, replayable against every peer the key is known to. General rule: ask of any authentication check whether the operation is once-per-peer or repeatable, and whether answering it costs anything. Also recorded: domain-separating a new signature type from an existing one, and why the freshness guard must burn its nonce only when it actually answers."
auto_inject: true
applicable_when: "Adding or reviewing any signature, binding, challenge, or admission check on a control-plane or handshake path; or when a check gates an operation that a peer performs repeatedly"
confidence: 1.0
verified_at: "10/05/2026"
verified_by: "dsh-agent"
staleness_signal: "If `binding_payload` gains a nonce, timestamp, or counterparty field; if directory queries become rate-limited or one-shot per registration; if a challenge/response handshake replaces self-attestation"
tags: [fact, security, cryptography, control-plane, replay, authentication, protocol-design]
edges:
  - target: fact-observer-stake-paradox
    type: related_to
    weight: 0.9
    note: "The reason the query path could not simply be opened up: without freshness, removing the gate removes the only thing the gate was enforcing"
  - target: fact-control-plane-peering
    type: related_to
    weight: 0.8
    note: "Register, RegisterAck and Heartbeat all ride the same static binding"
  - target: fact-mesh-verification-probe
    type: supports
    weight: 0.7
    note: "Mesh fragments put the timestamp inside the signature for the same reason: an undated proof cannot be judged stale"
related: ["[[Paying for observation in stake: the gate that makes monitors cost fault tolerance]]"]
source_url: "Empty"
---

# A signature over a static payload is a permanent credential

## The finding

`binding_payload` (`identity.rs:510-517`) signs exactly three things:

```rust
serialize_to_bytes_rmp(&(rhash, requested_type, requester_types))
```

No nonce. No timestamp. No counterparty. Self-attestation: "this Ed25519 key owns
this rhash and holds these roles."

For `Register` that is fine — it happens once per peer, and the answer is cheap.

The same payload gated `Request`, a directory query. A directory answer for 40
validators is ~156 KB of hybrid signatures plus an ML-DSA signature to produce, and
a direct-packet cap of 481 B (`wrapper.rs:79`) pushes all of it onto the
Resource-transfer path. So the shape was: **a ~200-byte request, repeatable
indefinitely, answered with an expensive artifact, gated by a credential that never
changes.** Anyone who observed one legitimate query from a staked node could replay
it forever, against every peer that key is known to. The gate looked like access
control and was, in the only dimension that metered cost, nothing at all.

The general question, worth asking of every check:

1. **Is the operation once-per-peer or repeatable?** A static proof is fine for the
   first and useless for the second.
2. **Does answering cost anything?** If yes, an unmetered repeatable operation with
   a permanent credential is a hole regardless of how good the signature is.
3. **Can the proof be pointed at someone else?** A credential not bound to its
   counterparty is portable across every peer you have.

## Domain separation is not optional

Adding a *second* signature type to a protocol that already has one creates a
smuggling problem: a signature accepted by the weaker gate can be spent at the
stronger one if both sign the same bytes. A directory query signature is now over a
five-tuple `(requester_rhash, responder_rhash, nonce, requested_type,
requester_types)` while a registration binding is over the original three — so the
payloads differ by shape, and a query signature cannot register anyone.

The part worth keeping is that this is **tested in both directions**
(`a_query_signature_cannot_register_and_a_registration_binding_cannot_query`), not
assumed. The direction that matters is the weak-to-strong one: the attacker in that
test holds no stake, and if a query signature could be spent as a `Register`, the
new observer-friendly path would have been a door around the stake gate that this
change was careful not to open.

When in doubt, add a distinguishing field to the payload rather than relying on
`request_type` to dispatch which signature is meaningful — the type field is
attacker-chosen.

## Ordering detail that changes behavior

The freshness guard burns a nonce when it records one. Where that check sits in the
sequence is a real decision:

```
target is us → signature valid → we can reach them → nonce unseen → answer
```

With the nonce check earlier, a query refused for a *transient* reason (a peer whose
route has not come up yet) consumed its nonce, converting "not yet" into "that query
is now permanently dead". The requester generates a fresh nonce per attempt so it
recovers, but the failure mode is exactly the kind that shows up as an unexplained
stall at 3 a.m. Burn the one-shot token only when you actually answer.

## Test environments must not be laxer than production

The reachability check consults the transport's destination table. Registry unit
tests run with `network = None`, and the first cut treated that as "cannot check →
allow". That would have made the fast suite **more permissive than any production
node** — a suite that passes for reasons production would not, which is worse than
no test because it launders the assumption.

The fix: fail closed, and give tests an explicit seam to seed reachability
(`seed_route_for_test`) so the honest sequence — cold query refused, in-contact query
answered — is asserted in the fast suite, while the announce path that really fills
the route table stays covered by the live-loopback tests. Single source of truth in
production; an explicit, deliberate stand-in in tests.
