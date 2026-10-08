---
id: log-multihost-phase2-selfdelivery-20261008-162000
type: log
operation: multihost-phase2-debug + defect-mitigation
date: "2026-10-08T16:20:00"
namespace: pneumatic
summary: "Landed the PreloadForFinalizer rename (user-approved option 1) end-to-end with mutation-verified guards and the real-RNS 8-node e2e; fixed the env-dir restart killer (loader parsed the node's own log as a spec); then instrumented the live cluster to PROVE the final blocker: composites have no self-delivery — send_to_all buckets hold peers only, each node rejects all 3 peers' forked commits and never receives its own (15/15 HASH_DIFFER, 0 COMMIT-OK); fact node + scorecard written, fix design sketched"
affected_nodes: ["fact-composite-no-self-delivery", "fact-composite-fanout-role-collision", "task-multihost-testnet-rollout", "_system/INDEX.md"]
tags: ["log", "multihost-phase2-debug", "defect-mitigation", "composite", "self-delivery", "config-loader"]
---

User approved distinct-hop-action (option 1) for the fan-out collision. Implementation:
`"PreloadForFinalizer"` adopted by both finalizer-bound senders (executor.rs stamped hop;
sentinel notifier pre-notify), admitted in FINALIZER_ACTIONS, routed in the finalizer
adapter to the pre-existing `handle_preload`; sentinel→executor keeps `"Preload"`.
Pinned four ways, two mutation-verified: the finalizer crate's `test_handle_preload` now
asserts the WIRE preload leaves the entry `Preloaded` (executable) in its OWN registry;
executor + sentinel tests pin the new wire name exactly; `finalizer_preload_action_reaches_
the_handler` fails UnknownAction when the admit is removed; `role_action_sets_are_pairwise_
disjoint` fails loudly when any two roles ever share a name (mutation: add Preload to
FINALIZER → caught). The 8-node real-RNS `e2e_standard_pipeline_commits_over_rns` — a
topology the single composite cannot fake — passes on the new name. Workspace 1195/0/37.

Re-up then died instantly on all four nodes: a config-loader trap — the env dir legitimately
holds the node's own `pneumatic.log`, and `get_environment_metadata_from` parsed EVERY file
as a spec and failed boot fail-closed on `[2026-…` log lines. Loader now treats only `.json`
as spec candidates (malformed `.json` stays fatal — the new test asserts BOTH halves;
mutation reproduces the exact `invalid type: integer 2026`). RUNBOOK troubleshooting gained
the symptom + old-image workaround.

Still 0 committed, so instrumented the live path instead of theorizing (diag image):
finalizer gate states, finalizer-adapter arrivals, commit-path acquire/booked-vs-wire,
COMMIT-OK sentinel. Arithmetic on 5 txs convicted a design gap, not a bug: per tx each of
the four finalizer roles optimistic-finalizes a DIFFERENT block (timestamp binds the hash)
and books `Committed{own_hash}`; each node receives exactly the THREE peers' Commit copies
(3× acquire_fail, 15/15 booked≠wire) and never its own — the only copy that could pass the
P10 wire-authoritative check. The composite has no self-delivery: `send_to_all` iterates
registered peers; nothing registers or loopbacks the host's own bucket membership. The
single-composite e2e never saw this because the harness relays every hop itself ("mesh
stand-in") — the tests were the missing capability. Authored `fact-composite-no-self-delivery`
(mechanism, the convicting numbers, the relay-blindness warning, the bridge-layer
self-subscription sketch, and the two proofs the fix must produce); INDEX + task scorecard
synced. Diagnostics (marked `[diag]`, incl. one verbose gate print) stay in the tree until
self-delivery lands; remove or demote them then. Phase 2 exit test remains unpassed — this
is the last known blocker between here and a growing chain.
