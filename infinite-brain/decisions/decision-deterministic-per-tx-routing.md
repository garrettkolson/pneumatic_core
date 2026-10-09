---
id: decision-deterministic-per-tx-routing
title: "ADR-004: Deterministic per-transaction routing (not epoch-wide leader)"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Each transaction routes to its own finalizer via deterministic_select(stakers, tx_id, epoch) over a stake snapshot frozen at the epoch boundary — the epoch-wide leader is a throughput bottleneck; the frozen snapshot eliminates state divergence."
auto_inject: false
applicable_when: "Changing finalizer assignment, the selection function, or snapshot handling"
confidence: 0.95
verified_at: "10/09/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when routing reverts to an epoch-wide leader or the seed/snapshot inputs change"
tags: [adr, design-decision, routing, determinism, throughput]
edges:
  - target: pattern-two-tier-snapshot-cache
    type: supports
    weight: 0.8
    note: "The tiered cache is the latency machinery this decision relies on"
  - target: concept-sentinel-role
    type: supports
    weight: 0.8
    note: "The sentinel is where per-tx assignment runs (assign_finalizer_deterministic)"
  - target: decision-deterministic-leader-election
    type: related_to
    weight: 0.7
    note: "Same seeded-SHA-256-sorted-walk algorithm; the seed is the tx id, not the epoch"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated verbatim from README ADR-004 when the README was trimmed (09/26/2026)"
  - target: fact-composite-per-host-fork-divergence
    type: related_to
    weight: 0.95
    note: "The primary path never applied this; the three rules below are what made it real (10/09/2026)"
  - target: fact-h12-assumes-lockstep-execution
    type: related_to
    weight: 0.9
    note: "One-producer routing is what makes the committer's local-vs-block result compare load-bearing — the open consequence"
  - target: decision-sentinel-routing-authority
    type: depends_on
    weight: 0.9
    note: "One decision per transaction only holds if the Sentinel that admits it is the only one who routes it"
related: []
source_url: "repo:README.md (former lines 122-126)"
---

# ADR-004: Deterministic per-transaction routing (not epoch-wide leader)

Each transaction is routed to a specific finalizer via `deterministic_select(stakers, seed_bytes, epoch_number)` where `seed_bytes = tx_id_bytes`. A stake snapshot frozen at the epoch boundary is the selection authority.

**Rationale**: An epoch-wide leader is a throughput bottleneck — only one node proposes per epoch. Per-transaction routing distributes work across all staked nodes. The stake snapshot eliminates state divergence: all nodes agree on the selection authority because it's persisted in `DataProvider` at epoch boundaries. The tiered cache (local → DataProvider → peer) minimizes network latency for the common case (local cache hit).

## What made it real (10/09/2026)

The design was sound and had never been applied: selection ran only in the rejection
path, every hop after it was a bucket broadcast, and no role compared anything to its
own identity (`fact-composite-per-host-fork-divergence`). Applying it across the chain
produced three rules that are now in the code and tested, and they are the parts a
future reader cannot re-derive from the ADR text:

1. **The snapshot decides weight; the registry decides addressability.**
   `Sentinel::staked_finalizers` selects over the epoch snapshot restricted to
   Finalizers this node can address: registered Finalizer peers, **plus this host's own
   key when the host self-delivers the Finalizer role**. The own-key case is not an
   edge — on a composite the peer bucket is legitimately empty (peering registers
   *peers*), so a bucket-only rule declares a single-host deployment un-routable. An
   empty candidate set is a refusal that names both counts, never a fallback to the
   whole stake set.
2. **One transaction, one decision.** `Sentinel::routing_decision_for` honours a
   record that arrives with the transaction instead of recomputing it. Every host runs
   a Sentinel; if each re-selected for itself, the environment held as many decisions as
   hosts and every enforcing gate passed on its own terms — which is what the four-host
   rehearsal measured before this rule existed.
3. **The record is verified against what the receiver holds.** A routing record travels
   inside the object it authorizes, so attestation + attester-role + sender-naming is
   satisfied by any self-routing sender. `Committer::held_decision` compares the
   incoming decision with the one this node's own registry holds. It is **not** covered
   by H12: `Transaction::hash()` is the canonical hash, and `selection` is deliberately
   outside `CanonicalTransaction` (it is not user intent; binding it there would let a
   Sentinel change what a transaction authorizes). Comparisons of two records use
   `SelectionRecord::same_decision_as`, never `==`, because hybrid signatures are
   randomized ([[fact-hybrid-signature-not-deterministic]]).

Verification is never signature-only: signature checks are vacuous against a Finalizer
that signs itself, so every enforcing role also resolves the attester to a registered
**Sentinel**. `selection: None` is not "unconstrained" — the enforcing roles refuse it.
The only exemption is the shielded path, and it is explicit in the committer.

## Residuals this decision does not close

- **Two Sentinels whose views differ can still choose differently.** The candidate set
  is a local view. The clean fix is a role-scoped `FinalizerSet` persisted per epoch
  beside the `ExecutorSet` — `deterministic_select_shard` takes executors from an
  `ExecutorSet` for exactly this reason; finalizers have no equivalent. Phase 7's
  "declare and verify the responsible set".
- **The record is trusted, not re-derived.** Recomputing `deterministic_select` from
  `epoch_number` + the carried `salt` would catch a lying Sentinel, but the committer
  has no epoch-keyed snapshot read, the finalizer reads current-epoch only, and a tip
  one block off selects a different node — so re-deriving on the commit path rejects
  honest blocks today.
- **A Byzantine Sentinel** signing two different decisions for one transaction is
  attributable (the attestation names it) but not prevented; ADR-007 already places
  routing authority in the Sentinel.
- **Convergence is not routing.** With one producer per transaction, the committer's
  H12 compare demands the receiver's local execution equal the block's — see
  [[fact-h12-assumes-lockstep-execution]]. Rehearsal state as of 10/09/2026: one host
  grows, three refuse. That is the next decision, not a routing gap.
