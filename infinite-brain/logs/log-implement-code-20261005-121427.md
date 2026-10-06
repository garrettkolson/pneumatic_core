---
id: log-implement-code-20261005-121427
type: log
operation: implement-code
date: "2026-10-05T12:14:27"
namespace: pneumatic
summary: "Built mesh-probe, the instrument for the rollout's Phase 2: it judges a snapshot of node role-directories against the cluster's own manifest.json and exits non-zero on any discrepancy. Two greps decided the design — successful registrations log nothing (so no log-scraping check can work), and registration runs a stake gate (so a probe must be a genesis-staked node). Silence gets its own exit-1 PARTIAL finding, distinct from an empty directory. Judgment is pure and mutation-verified at two layers. Suite 1101 → 1118/37/0"
affected_nodes: ["fact-mesh-verification-probe", "fact-test-suite-mesh-probe", "task-multihost-testnet-rollout", "fact-control-plane-peering", "fact-fanout-graph-density", "repo:testnet-gen/"]
tags: ["log", "implement-code", "probe", "mesh", "observability", "deployment", "testing"]
---

Picked up the rollout's Phase 2 instrument after the user chose it over the
launcher, the gateway, and Terraform — the right order, since it's the piece that
tells you whether the transport is viable at all.

Two greps before writing anything, and both changed the design:

1. **Successful registrations log nothing.** `registration.rs` emits on rejection
   paths only (`:274`, `:284`, `:504`, `:550`); an admitted, installed peer is
   silent. So log-scraping was never going to work — a cluster that never formed
   and one that formed perfectly are byte-identical in the logs. This is the third
   member of the same family found this week (loopback bind, wrong genesis key,
   reversed j-rule): the node that looks healthy and peers with nobody.
2. **Registration runs a stake gate** (`:434`). A probe can't be a lightweight
   outsider — it must be a genesis-staked node or every registration is refused
   with "insufficient stake", which reads exactly like a mesh that never formed.
   Checking this cost one grep; assuming it would have cost a live multi-host run
   and a wrong conclusion about the transport.

The judgment is a pure function — mesh in, snapshot in, findings out — because
that's the part with the most ways to be wrong and the part testable without
hosts. The collector that fills snapshots is deliberately separate; it's the piece
that needs machines.

Three choices I'd defend again:

- **Judge the manifest, don't re-derive the topology.** `Mesh::from_manifest`
  rebuilds what was generated. Re-deriving from the flags typed today silently
  disagrees with the cluster on disk — that's a "200 edges missing" report whose
  real cause is different counts. And a manifest naming an unknown peer **fails
  the reload**: a reader that quietly dropped a peer would make the probe
  under-report, i.e. go green on a broken cluster.
- **Silence is not emptiness.** A host that never answered gets `NotReported`,
  exit 1, and a `PARTIAL` banner. Conflating it with an empty directory is how a
  dead host gets read as a healthy one — which is worse than having no probe.
- **A green run says what it doesn't cover.** The success message itself carries
  "control-plane formation only — reachability needs traffic", so the caveat can't
  be lost by whoever reads the output. Expectations come from the loaded topology,
  so a role-graph cluster is *complete* rather than missing intra-executor edges —
  tested with two executors, the smallest fixture where that sparsity exists at
  all. The first version of that test had one executor and was therefore vacuous;
  caught while writing, not after.

Also extracted the CLI flag helpers into one module shared by both binaries rather
than copying them into the probe — same failure mode as the duplicated listen-IP
rule, which is how one binary ended up binding loopback and the other not.

Mutation-verified: suppressing the `Missing` check fails the unit test **and** the
CLI test independently, so an always-green probe can't be introduced unnoticed.
File restored byte-identical. Suite 1101 → **1118/37/0**.

What's left for Phase 2: the **collector** — a staked node that boots, peers,
issues directory Requests, and writes the snapshot. The format is already a
contract exercised from both sides.
