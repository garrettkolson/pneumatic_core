---
id: fact-mesh-verification-probe
title: "Mesh health is self-reported and signed; logs cannot answer it"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/05/2026 (revised same day): successful registrations log nothing — only rejections emit (registration.rs:274/:504/:550) — so a cluster that never formed and a healthy one are identical in the logs. Verification must come from nodes, and the shape that works is a per-node signed self-report (mesh fragment) shipped by the log pipeline and judged by `mesh-probe`: no collector node, no stake, no mesh-wide links. The earlier 'staked collector queries directories' design is superseded by fact-observer-stake-paradox."
auto_inject: true
applicable_when: "Verifying a running cluster's mesh, building observability for nodes, or debugging a cluster that appears healthy but is not peering"
confidence: 1.0
verified_at: "10/05/2026"
verified_by: "dsh-agent"
staleness_signal: "If the successful registration path starts emitting a log line, if `delivery_failures` or the registry's `last_seen` semantics change, if the fragment format version moves past 1, or when the fragment's `vouched`/age fields are actually consumed by an alert"
tags: [fact, observability, mesh, probe, fragments, registration, deployment, testing]
edges:
  - target: fact-observer-stake-paradox
    type: depends_on
    weight: 0.95
    note: "Why the design is self-report rather than a staked collector node"
  - target: fact-control-plane-peering
    type: depends_on
    weight: 0.9
    note: "The role directories a fragment reports are what peering builds"
  - target: fact-transport-loopback-bind-default
    type: supports
    weight: 0.8
    note: "Same failure family: a node that looks healthy and peers with nobody, invisible in logs"
  - target: fact-fanout-graph-density
    type: related_to
    weight: 0.8
    note: "Fragments reveal the interface count a node really holds, and delivery failures are the relay-vs-fewer-nodes signal"
  - target: task-multihost-testnet-rollout
    type: supports
    weight: 0.9
    note: "Phase 2's instrument and Phase 5's first real observability surface"
  - target: fact-register-ack-bucket-placement-defect
    type: related_to
    weight: 0.7
    note: "SelfPresent and WrongBucket findings exist to catch exactly this defect class"
related: ["[[The peering initiator: how a node registers with its bootstrap peers]]"]
source_url: "Empty"
---

# Mesh health is self-reported and signed; logs cannot answer it

## The finding that decided everything

**The successful registration path emits nothing.** `registration.rs` logs
rejections — binding failure (`:274`), unregistered requester (`:284`), ack binding
failure (`:504`), "no bucket took it" (`:550`) — but a peer that is admitted and
installed is silent. So **a cluster that never formed and one that formed
perfectly are byte-identical in the logs.** Any log-scraping health check reports
green on a dead mesh. This is the third member of a family found while working the
transport: the loopback bind, the wrong genesis key, the reversed j-rule — all the
healthy-looking non-peering node.

Verification has to come from the nodes themselves.

## Two designs, one of which was wrong

The first shape was a **collector node** that registers with everyone, queries
directories over the control plane, and writes one snapshot. It was built that way,
and two checks killed it:

- **Stake.** Registration is stake-gated, so a collector must be a genesis-staked
  node — and stake is quorum budget (`fact-observer-stake-paradox`). An observability
  tool would have been paid for in fault tolerance, and it would have been
  selectable as a leader while doing no work.
- **Density.** Leaves cannot route through other leaves, so a collector could only
  ever observe nodes it held *direct links* to: 39 interfaces at 40 validators.
  The interface ceiling moved onto the monitoring box.

What replaced it is a **mesh fragment**: each node signs a self-report of its own
role directories, writes it to a path from `config.json`, and the log/metrics agent
that already runs ships it. A node describing itself needs no registration, no
links, and no stake. `mesh-probe` aggregates fragments and judges them against
`manifest.json`.

**Centralize nothing but the arithmetic.** The only centralized thing left is a
pure function over signed artifacts, so the verdict is recomputable by anyone from
the same files. Withholding is still possible — but it is *visible*: a missing
fragment is `not-reported`, exit 1, `PARTIAL`. Monitoring should not make
withholding impossible; it should make it impossible to mistake for health.

## What a fragment carries, and why each field

| Field | Why it is there |
|---|---|
| `buckets` → peers with `rhash_hex` | The directory contents, which is the primary question |
| `vouched` | "I registered this node myself" vs "someone told me about it". A full bucket of unvouched peers is a cluster that learned names and has no direct links — which does not route |
| `last_seen_age_secs` | Eviction drops a peer not seen for 30 s (`registry.rs:328`, 1 s passes), so a large age means the entry is already fiction. A bucket count cannot tell a live cluster from a dying one |
| `delivery_failures` | Failed fan-outs per peer, already tracked by the registry for fan-out observability. This is the only reachability evidence available: **listed and unreachable** is otherwise indistinguishable from peered |
| `written_at_unix` | **Inside the signature**, so age is provable and a re-dated fragment fails verification. Otherwise the freshness bound is decorative |

Signing is the hybrid `(Ed25519 · ML-DSA-44)` the control plane uses (`crypto.rs:126`:
both halves must verify), so an edited fragment is refused rather than trusted.

## Three things a green report does not mean

1. **Not reachability.** Delivery failures make unreachability visible; a node
   cannot report a path it has not had to use. Send traffic.
2. **Not "the whole cluster".** `is_complete()` requires no findings **and** no
   rejected evidence. A report built from 4 of 12 fragments is a clean report of a
   quarter of a cluster.
3. **Not current.** Fragments past `--max-age` (default 30s ≈ one eviction window)
   are discarded and counted as rejected. An unbounded age turns a stopped pipeline
   into a permanently healthy dashboard.

Rejections print apart from findings, because "3 nodes disagree with the topology"
and "3 fragments were unverifiable" need different people and different fixes.

## Verification

Core: 8 unit tests — a dump reports what the registry holds and verifies under its
own key; unvouched vs registered peers; tampering breaks the signature; **a
refreshed timestamp does not revive a stale fragment**; verification uses the
consumer's key; unsigned fragments never reach disk; **two dumps of one state agree
byte-for-byte** (DashMap iteration order is not guaranteed, so unsigned ordering
would make the same mesh verify differently); bucket-name pinning.

`testnet-gen`: 2 end-to-end tests over the real chain — generated
`config.json` → `Config::build()` → real `NodeRegistry` → `MeshFragment::dump_to` →
`Fragments::load` → verdict complete at 12/12 edges; then the same files judged too
late (all stale), tampered (bad signature), foreign (unknown reporter), and
re-signed with real delivery failures (unreachable, and the node drops out of
`verified`). Plus 3 CLI tests (empty fragment directory ⇒ whole cluster
unobserved; two evidence sources ⇒ usage fault; stale directory ⇒ rejected +
partial) and a cross-check pinning core's `bucket_key` to the generator's
`Role::plural()`.

One test caught a live inconsistency in the judge while doing this: with no
evidence at all, the report advertised **0 expected directed edges**, because the
denominator was counted per reported node. It now comes from the topology, so an
unobserved fleet prints "12 expected / 0 observed" — which reads as unchecked
rather than as nothing-to-check.

## Superseded

The earlier version of this node said the probe "must be a genesis-staked node" and
that a collector was the missing piece. Both were true of the collector design and
both are gone: **self-report removed the stake requirement and the density ceiling
at once.** See `fact-observer-stake-paradox` for the mechanism.
