---
id: task-s5-4-real-pool-swap
title: "S5.4 — Composite node-server: real Arc<ShieldedPool> wiring"
type: task
namespace: pneumatic
visibility: namespace
summary: "Replace the composite node's SimpleShieldedPoolView with the real shared Arc<ShieldedPool> in the node-server DI bundle, and add SignShielded/ShieldedVote to the finalizer role's action set."
auto_inject: false
applicable_when: "Implementing or reviewing S5.4, the final wiring phase of the shielded stack in the composite runtime"
confidence: 0.95
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Close when the S5.4 entry lands in AUDIT_CHECKLIST.md with its Done note"
tags: [task, shielded, node-server, pool, wiring, s5]
edges:
  - target: source-shielded-plan
    type: derived_from
    weight: 1.0
    note: "Specified in the S5.4 section (lines 905-941)"
  - target: concept-shielded-pool-view
    type: depends_on
    weight: 0.9
    note: "The stub view being swapped out for the real shared pool"
  - target: concept-shielded-pool
    type: depends_on
    weight: 0.8
    note: "The real pool (tree + nullifiers + applied-update map) becomes the shared artifact"
  - target: event-node-server-composite
    type: related_to
    weight: 0.8
    note: "The wiring lands inside the composite role-plugin runtime"
  - target: event-s5-2-finalizer-wiring
    type: related_to
    weight: 0.8
    note: "Adds the SignShielded/ShieldedVote finalizer actions S5.2 defined"
  - target: task-s5-3-pool-append
    type: related_to
    weight: 0.7
    note: "The committer write path it threads through presumes S5.3's guarded pool"
  - target: pillar-shielded-value-transfer
    type: part_of
    weight: 0.9
    note: "Completes the S5 wiring phase of the Tier-1 plan"
related: []
source_url: "repo:pneumatic-shielded-implementation-plan.md"
---

# Task: S5.4 node-server role wiring (real pool swap)

Specified in the shielded implementation plan, **S5.4 — Node-server role wiring** (lines 905–941).

Actions: (1) `FINALIZER_ACTIONS` (`node_server.rs:43`, currently `&["Sign", "Finalize"]`) gains `"SignShielded"` and `"ShieldedVote"`, with new `handle` match arms; the existing `"Sign"` arm and the fail-closed else arm are untouched. (2) `SENTINEL_ACTIONS`/`COMMITTER_ACTIONS` stay **unchanged** — shielded transfers ride the `Verify` envelope and shielded commits the existing `Commit`/`BlockFinalized` actions. (3) `build_runtime` (:204–402) constructs the `ShieldedPool` (S1.6 tree + S4.2 nullifier registry + applied-update map, under the single-writer guard) as a shared **`Arc`** in the DI bundle — replacing the `SimpleShieldedPoolView` stand-in — and `build_role_plugin` (:409–520) threads it into the Sentinel plugin (read-only), the Finalizer plugin (read-only, for the K-recency check), and the Committer/`BlockServices` (write path, S5.3). (4) `route_data_plane` needs no special-casing — shielded payloads are ordinary length-prefixed messages (~4.6 KB typical, well under the 16 MiB frame cap).

Verify: composite node routes a shielded tx end-to-end in the in-process pipeline test; unknown-action rejection preserved; discriminator: removing `"SignShielded"` from `FINALIZER_ACTIONS` in a test build → vote requests rejected `UnknownAction`, proving the gate is load-bearing.
