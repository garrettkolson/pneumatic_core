---
id: fact-register-ack-bucket-placement-defect
title: "RegisterAck filed the responder under the requester's role"
type: fact
namespace: pneumatic
visibility: namespace
summary: "RESOLVED 10/02/2026: handle_register_ack installed the peer under the ack's node_type, which names the type the *requester* was registered under — so a finalizer filed its committer peer into its own Finalizer bucket. The responder is now filed under the role set it declared for itself in the ack, which the binding covers."
auto_inject: true
applicable_when: "Debugging mis-routed role traffic (a role sending to peers that cannot serve it), or changing registration ack semantics"
confidence: 0.35
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale while handle_register_ack installs under `request.requester_types` (the responder's own declaration) with the acked-type fallback for an empty set"
tags: [fact, node-registry, registration, ack, routing, defect, resolved, testnet]
edges:
  - target: concept-node-registry
    type: related_to
    weight: 0.95
    note: "The defect sits in handle_register_ack (registration.rs), the client half of the registration protocol"
  - target: fact-control-plane-peering
    type: related_to
    weight: 0.85
    note: "Only reached once something actually sent a Register; the e2e test uses asymmetric roles specifically to pin it"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.5
    note: "Silent misrouting: every node looked registered, nothing errored, role traffic went to peers that could not serve it"
related: ["[[NodeRegistry: per-type peer directories + signed registration protocol]]"]
source_url: "src/node/registry/registration.rs (handle_register_ack)"
---

# RegisterAck filed the responder under the requester's role

Code- and socket-verified 10/02/2026; fixed same day.

**The two meanings of one field.** When B processes A's `Register`, it admits A
under every type A declared, then acks with `node_type` = the highest-priority
type **A** was registered under (`handle_register`, registration.rs). A's
handler then did:

```rust
self.register_peer(responder_key, responder_rhash, &node_type, conn)  // ← A's type, B's row
```

So a finalizer that registered with a committer stored *the committer* in its
**Finalizer** bucket. Every node then sent `Sign` traffic to peers that could not
serve it, and still found nobody in the buckets it actually needed — while every
directory looked populated and every ack verified.

**Why it survived: the handler had zero test coverage.** Verified with
`git grep RegisterAck HEAD` — the only test in the tree that mentioned the variant
was `node_request_register_ack_round_trip` (`src/node.rs:277`), which serializes a
`RegisterAck` and reads the fields back. A wire-format test. **Nothing ever passed
a `RegisterAck` to `handle_register_ack`.** No integration test reaches it either:
`tests/pipeline_integration.rs`, `tests/shielded_pipeline.rs` and
`tests/transport_integration.rs` build their cluster topology by calling
`register_peer(key, rhash, node_type, conn)` directly, choosing each peer's bucket
by hand — and `register_peer` has **no non-test caller** (every reference in the
worker crates sits inside a `#[test]`). Those tests hand-build the exact directory
that production is supposed to *derive* from a signed exchange, so the derivation
layer is invisible to them. Worth naming as a general trap: an integration test
whose fixture replaces the wiring under test can pass indefinitely while the
wiring is broken.

**Fix.** File the responder under the role set **it declared for itself** in the
ack (`request.requester_types`) — the same field its binding signature covers, so
a peer cannot claim a role it did not sign for — one insert per declared type,
capacity-gated. A responder declaring nothing (no compliant responder does)
falls back to the acked type rather than vanishing. The ack's binding is also now
stored, so an ack-learned peer is *vouchable*: `handle_request` lists only nodes
with a non-empty `directory_signature`, and without this a cluster could never
grow past the peers it bootstrapped with.

**Pinned** by `an_ack_files_the_responder_under_the_roles_it_declared` (unit),
`two_nodes_peer_over_udp_and_land_in_each_others_role_directories`
(`tests/peering_e2e.rs`), and
`a_cluster_derived_by_peering_routes_the_pipelines_real_hops`
(`tests/peered_topology_e2e.rs`), which gives the two nodes *different* roles
precisely so the old behavior cannot pass. The last one is mutation-verified:
reinstating this defect makes it fail with the sentinel holding two entries in
its own Sentinel bucket (`fact-integration-test-fixture-blind-spot`).
