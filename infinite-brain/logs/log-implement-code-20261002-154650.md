---
id: log-implement-code-20261002-154650
type: log
operation: implement-code
date: "2026-10-02T15:46:50"
namespace: pneumatic
summary: "Answered a challenge to the RegisterAck finding by verifying it rather than restating it: the root integration tests never executed the ack path (git grep RegisterAck HEAD → one serde round-trip test), and all three seed their directories via register_peer, a test-only API with no non-test caller — so they could not observe the wiring. Closed the gap with tests/peered_topology_e2e.rs (4-node full mesh, directories derived by peering, pipeline hops routed over them, 2.1 s, mutation-verified against the ack defect); corrected a wrong claim made earlier today in fact-register-ack-bucket-placement-defect; suite 1064 → 1065/37/0"
affected_nodes: ["fact-integration-test-fixture-blind-spot", "fact-test-suite-peer-derived-topology", "fact-test-suite-peering", "fact-register-ack-bucket-placement-defect", "task-testnet-launcher", "repo:tests/peered_topology_e2e.rs"]
tags: ["log", "implement-code", "testing", "integration-tests", "mutation-testing", "peering", "correction"]
---

The user asked a question with a false premise embedded in it: *if* the ack
misfiling were real, how were the e2e tests passing — and if they were passing
erroneously, why keep them? Both halves deserved verification rather than a
plausible retelling, so I went to the code.

The premise dissolved: those tests were not passing on buggy logic, they were
passing on a path that did not contain the logic. `git grep RegisterAck HEAD`
returns one test, a msgpack round-trip of the enum. Nothing ever called
`handle_register_ack`. The tests document this themselves —
`pipeline_integration.rs:306`: "this test sends no control-plane traffic — peers
are registered directly in each `NodeRegistry`." Following that thread produced
the real finding: `register_peer` has **no non-test caller**, so every multi-node
test manufactures the directory that production is supposed to derive, and no
test can see the derivation.

A second, humbler finding: **the explanation I had written in the vault that same
day was wrong.** I had claimed existing ack tests "used a single role on both
sides," inferring it instead of checking. Reality was zero coverage. Corrected in
the fact node. Today's log node keeps the wrong claim, since logs are append-only
— the correction lives with the durable statement, and the mistake is visible
where it was made.

On whether to keep the older tests: yes, and the answer needed the code to say so.
They pin pipeline semantics against a fixed directory, which is a legitimate
fixture choice; their defect is scope presented as coverage, not the logic they
exercise. Deleting them would trade one blind spot for a regression surface. The
fix is a test that cannot dodge the wiring, so I built that rather than editing
the ones that do — converting a 1199-line test whose value is orthogonal would
have traded real coverage for a different kind of flakiness.

Two implementation facts made the new test cheap: production admits peers with a
transport-backed `RnsConnection`, so a derived directory is a *usable* one; and
`RnsSender::get_response` is fire-and-forget ("no response expected"), so the real
`send_to_all_blocking` returns immediately instead of burning a 5 s bound per hop.
Wrote the mesh with the j-rule the older test documents, and it assembled on the
first run — 12 routes live, all directories derived, all four pipeline hops
delivered, 2.1 s.

My first assertion was wrong, not the code: I expected every bucket to hold one
entry, but a node is not its own peer, so each node's home-role bucket is empty.
The corrected assertion (one per foreign bucket, zero in the home bucket) is the
stronger property — it also rules out self-registration.

Then the step that decides whether the test is worth anything: reinstating the
defect made it fail with `[sentinel] bucket Sentinel should hold 0, left: 2`, then
restored clean and re-greened. A test never observed failing on the bug it claims
to catch has not itself been tested. Suite 1064 → 1065/37/0, new test stable across
four runs.

Remaining and recorded: the three older multi-node tests still seed at 6 sites,
and converting them is easier once a launcher can express a peer/port matrix —
which is the same object the key/genesis generator has to produce anyway, so the
two open items on `task-testnet-launcher` have now converged on one piece of work.
