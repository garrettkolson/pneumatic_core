---
id: log-implement-code-20261005-134144
type: log
operation: implement-code
date: "2026-10-05T13:41:44"
namespace: pneumatic
summary: "Replaced the centralized mesh collector with decentralized self-reporting after the user raised the single-point-of-failure concern: each node signs a mesh fragment (its own role directories, plus per-peer delivery failures and last-seen age) to a path from config.json, and `mesh-probe --fragments` aggregates and judges. Checking the SPOF worry surfaced a worse one — the stake gate means an observer pays for visibility in quorum budget — which is what made self-report the right shape rather than just a nicer topology. Centralize nothing but the arithmetic. Suite 1118 → 1132/37/0"
affected_nodes: ["fact-observer-stake-paradox", "fact-mesh-verification-probe", "fact-test-suite-fragments", "concept-env-driven-config", "task-multihost-testnet-rollout", "fact-test-suite-mesh-probe", "repo:src/node/registry/fragment.rs", "repo:testnet-gen/"]
tags: ["log", "implement-code", "fragments", "observability", "mesh", "staking", "quorum", "deployment", "testing"]
---

The user worried that a centralized collector makes network health a
single point of failure. Worth taking seriously, and worth checking rather than
arguing about.

Sharpening it first: the collector is not a consensus SPOF — if it dies, blocks
keep coming. What it centralizes is *the verdict*. And integrity was already
decentralized, since directory responses and entries are signed; a collector can
withhold, not forge. So the real question was whether the availability half was
worth its cost.

Then two checks, and the second one reframed everything:

- **Density.** Leaves can't route through leaves, so a collector could only observe
  nodes it holds direct links to. 40 validators ⇒ 39 interfaces on the monitoring
  box. The interface ceiling we keep hitting on validators would have moved onto the
  thing that watches validators.
- **Stake.** Registration is stake-gated, and the same stake pool is both the
  validator set (`to_stake_set()`) and the quorum denominator (`quoruming.rs:55`).
  So an observer either has no stake and sees nothing, or holds stake and sits in
  the denominator forever without voting. 4 validators × 1000 @67% need 2680/4000;
  add one 1000-stake observer and they need 3350/5000 while being able to reach only
  4000 — still finalizing, but silently down from tolerating one failure to
  tolerating none. **A monitoring tool paid for in fault tolerance.**

That second one is the reason self-report isn't merely a nicer topology — it's the
only shape that doesn't pay. Design rule recorded: *observation should not require
participation.* The open protocol question I'd settle next: should the directory
query require stake at all? It needs a binding signature and liveness, not a stake
position — and as written, every future observer, explorer, and load generator is
forced into the validator set. A load generator that inflates the quorum
denominator is a self-inflicted halt.

What landed: `MeshFragment` in core (buckets with `vouched` + `last_seen_age_secs`,
per-peer `delivery_failures`, timestamp **inside** the signature so staleness is
provable), one helper both binaries call so "a path in config means reporting is
on" lives in one place, `Fragments::load` in testnet-gen reusing the core type
rather than re-declaring it, and `--fragments` on the probe with a freshness bound.

Three judgement calls worth the ink:

- **Shape, not filename**, decides what's a fragment. Name-based filtering would
  have either swallowed `genesis.json`/`env/env.json` as broken fragments (noise
  that trains people to ignore the rejection list) or required a filename the
  operator can't control after a metrics agent renames files in transit.
  Unparseable JSON still counts as a broken fragment — that's a half-written file,
  and a node vanishing from the report silently is the exact failure this exists to
  catch.
- **Fragments replace the reachability hand-wave.** I'd been printing "reachability
  needs traffic" as a caveat; delivery failures turn half of that into evidence.
  Listed-and-unreachable is now a finding. Still not *proof* of reachability — a
  node can't report a path it hasn't had to use — and the module doc says so.
- **Verified must be revisitable.** `evaluate()` computes `verified` knowing nothing
  about delivery failures, so folding reachability in without recomputing would
  print a node as both verified and unable to reach its peers. Extracted one
  `Finding::concerns()` rule both paths share.

Three things my own code got wrong, caught by tests:

- `--fragments` over a real cluster printed **"0 expected directed edges"** for a
  12-edge mesh, because `expected_edges` was accumulated per *reported* node. An
  unobserved fleet advertised nothing to check. The denominator now comes from the
  topology. The old behaviour would have looked fine forever — it only shows up in
  the exact situation where the tool matters most.
- The fragment loader's `files_seen` counted sibling JSON as fragments, so the
  "only fragments counted" assertion failed before the design flaw behind it was
  named.
- Two invented APIs in my first draft of the e2e test (`from_slice_for_test`,
  `NodePlan::dir`). `dir` lives in the manifest, not the plan — which is the right
  place, since the manifest is the operator-facing record.

Adding a field to `Config` broke **18 exhaustive `Config { … }` literals across 12
files in five crates**. Mechanical, but `cargo check -p <crate>` doesn't reveal
them — only a workspace build. Recorded so the next `Config` field budgets for it.

The end-to-end test is the part I'd keep if forced to choose: generated
`config.json` → `Config::build()` → real `NodeRegistry` → `dump_to` →
`Fragments::load` → verdict, then the same files aged, tampered, forged as another
node, and re-signed with real delivery failures. Those are the wiring failures a
unit test can't reach: fragment lands where nobody reads, signs under a key the
manifest doesn't list, or arrives with bucket names the judge doesn't look up.

Suite **1132 / 37 / 0**. Collector is gone; the SPOF worry and the stake problem
went away together, which is usually a sign the shape was wrong rather than the
parameters.
