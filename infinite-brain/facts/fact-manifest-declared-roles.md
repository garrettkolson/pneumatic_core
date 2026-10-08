---
id: fact-manifest-declared-roles
title: "The manifest's per-node `roles` is a stake derivation — what a composite runs, not what it is named"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/08/2026: mesh-probe reported 36 wrong-bucket findings on a fully-formed mesh. The manifest declared one role per node, but the composite installs one plugin per role its stake qualifies for (stake 1000 vs floors 10 → all four) and declares them all — honest directories disagreed with a fictional manifest. `testnet-gen` now derives each node's `roles` with the same predicate the composite uses (`meets_minimum_stake` against the env template's floors), refuses a stake qualifying for nothing, and `mesh-probe` expects each peer in every declared bucket; `observed_edges` counts links (a peer held in ≥1 bucket), not bucket memberships."
auto_inject: false
applicable_when: "Reading or generating a manifest, writing probe expectations, or changing stake floors — the derivation must track `RoleSelector`"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "If `RoleSelector::select`'s qualification predicate changes without `qualifying_roles` in emit.rs, if a manifest hand-edit sets `roles: []`, or if probe expectations revert to the naming role"
tags: [fact, testnet-gen, manifest, roles, composite, mesh-probe, stake]
edges:
  - target: concept-node-server-composite-runtime
    type: derived_from
    weight: 0.95
    note: "The composite's selection-by-stake is the runtime behavior the manifest must faithfully record"
  - target: fact-control-plane-peering
    type: related_to
    weight: 0.8
    note: "declared_roles is what a Register advertises; the manifest now predicts exactly that"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.85
    note: "Wrong-bucket was one of the four rehearsal verdict defects; this closed it"
---

# The manifest records what nodes run, not what they are called

## The 36-finding fiction

`testnet-gen` writes one role per node (the naming role); the probe expected each
peer in exactly that bucket. The runtime never honored that: a composite's
`RoleSelector` installs every role its stake clears — floors are 10 by default and
generated genesis pays 1000 — so every node declares all four roles, lands in all
four buckets everywhere, and the judge scored 36 correct observations as wrong.
The directories were right; the manifest was wrong.

## The rule now

- `qualifying_roles(stake, template)` in `emit.rs` mirrors the composite's own
  predicate: `meets_minimum_stake(stake, global_min, type_min)`, floors read from
  the same env template the nodes boot with, defaults from
  `CostModel::default_global_min_stake` / `Config::default_min_stake`.
- A stake qualifying for **nothing** is refused at generation: every node would
  boot running no role — a cluster with no pipeline (the failure mode the old
  single-role manifest could not even express).
- `Mesh::from_manifest` treats `roles` as authoritative, falls back to `[role]` when
  absent (hand-made manifests stay valid), rejects `roles: []` as a contradiction.
- The probe's expectations are per-declared-role (missing/wrong-bucket), and
  `observed_edges` counts **links** — observer holds peer in ≥1 bucket — so it stays
  comparable to `expected_edges` (a four-bucket honest peer is one edge, not four).

Live proof: after the change, the generated manifest reads `roles:
[sentinel, executor, finalizer, committer]` on every node and the four-namespace
rehearsal probe went **exit 0, 12/12 edges, zero findings** with the same full
directories that had produced 36 wrong-bucket reports.
