---
id: fact-integration-test-fixture-blind-spot
title: "Integration tests seeded their own topology, so they could not see the wiring"
type: fact
namespace: pneumatic
visibility: namespace
summary: "RESOLVED 10/02/2026: pipeline_integration, shielded_pipeline and transport_integration all built their cluster directories with register_peer — a test-only API with no production caller — so no test could observe registration, ack handling or control framing. tests/peered_topology_e2e.rs now derives a 4-node mesh by peering and is mutation-verified against the RegisterAck defect."
auto_inject: true
applicable_when: "Writing or reviewing any multi-node test, or asking why a green suite failed to catch a wiring bug"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "If the root integration tests stop calling register_peer (they currently call it at 6 sites), or if peered_topology_e2e is removed or reverted to hand-seeded directories"
tags: [fact, testing, integration-tests, fixture, node-registry, peering, mutation-testing]
edges:
  - target: fact-register-ack-bucket-placement-defect
    type: supports
    weight: 0.9
    note: "The blind spot is why that defect survived; the new test pins it and was verified to fail with the defect reintroduced"
  - target: fact-control-plane-silent-drop-paths
    type: supports
    weight: 0.85
    note: "Three of the four silent-drop paths were unreachable from a hand-seeded directory"
  - target: concept-node-registry
    type: related_to
    weight: 0.8
    note: "register_peer vs admit_node_under_type: the test-only seeding path vs the production derivation path"
  - target: fact-control-plane-peering
    type: depends_on
    weight: 0.8
    note: "Deriving a topology requires a working initiator, which did not exist before 10/02/2026"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.5
    note: "A fixture that manufactures the answer is a fail-open test: it cannot report a wiring failure"
related: ["[[NodeRegistry: per-type peer directories + signed registration protocol]]"]
source_url: "tests/peered_topology_e2e.rs"
---

# Integration tests seeded their own topology, so they could not see the wiring

Verified 10/02/2026 against the code at HEAD, in answer to a direct question: if
the `RegisterAck` misfiling were real, how were the e2e tests passing? Answer:
**they never executed that code.** `git grep RegisterAck HEAD` across the whole
tree finds one test, `node_request_register_ack_round_trip` (`src/node.rs:277`),
which round-trips the enum through msgpack. Nothing passed a `RegisterAck` to
`handle_register_ack`.

The reason is the fixture. `tests/pipeline_integration.rs` says so about itself
(lines 303-307): *"this test sends no control-plane traffic — peers are
registered directly in each `NodeRegistry`."* All three multi-node tests seed via
`register_peer(key, rhash, node_type, conn)`, at 6 sites, with the test author
choosing each peer's bucket. **`register_peer` has no non-test caller** — every
reference in the worker crates sits inside a `#[test]`. Production derives
directories through `handle_register` → `admit_node_under_type` (binding
verification, stake gate, capacity lock) and `handle_register_ack` →
`register_peer_with_binding`. The tests manufacture the output of that derivation
and hand it to the code that consumes it.

Two properties make this worth fixing rather than annotating:

- **Delivery is addressed from the directory.** `send_to_all_blocking` ignores the
  stored `Connection` on the RNS branch and fans out to the **rhashes in the
  bucket** (`fanout.rs:91-111`). So a wrong bucket is a misrouted pipeline — the
  exact failure the ack defect caused, and invisible to a seeded directory.
- **The seeding API is not a production API.** A fixture built on an API nothing
  ships can stay green while everything shipped is broken.

**The fix** (`tests/peered_topology_e2e.rs`, 2.1 s, stable across 4 runs): four
nodes, one per role, full mesh over UDP (3 interfaces each — a chain is
impossible because leaves cannot route through other leaves, and control frames
need live routes). Nothing calls `register_peer`; every directory entry arrives
because a signed `Register` was verified, staked and bucketed. The test then
drives the pipeline's real hops — `Preload`, `Sign`, `Commit`, `Clear` — through
production `send_to_all_blocking`, naming only a role and an action, never a
destination address, and asserts each message lands at the receiver.

**Mutation-verified.** Reinstating the defect (`install_types = vec![node_type]`)
makes it fail with `[sentinel] bucket Sentinel should hold 0, left: 2` — the
sentinel filing two peers in its own bucket. Restored and re-greened. A test that
has never been observed failing on the bug it claims to catch has not been tested.
