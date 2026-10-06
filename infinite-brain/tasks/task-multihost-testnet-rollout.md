---
id: task-multihost-testnet-rollout
title: "Multi-host testnet rollout: from a working local cluster to production"
type: task
namespace: pneumatic
visibility: namespace
summary: "OPEN — phased. A correctly configured cluster can boot and peer on one host; it cannot be driven from outside and forgets its validator set on restart. Six phases with testable exits: (1) transaction ingress, (2) transport viability off loopback, (3) durable stake, (4) enforcement and key custody, (5) operability, (6) public testnet and review. Phase 2 is the riskiest unknown and must not be deferred."
auto_inject: true
applicable_when: "Planning or executing multi-host / cloud testnet deployment, deciding what to build next, or sizing a validator fleet"
confidence: 0.85
verified_at: "10/05/2026"
verified_by: "dsh-agent"
staleness_signal: "Any phase exit being met — especially an ingress surface existing, stake becoming durable, or the transport being exercised across real hosts. Re-verify the Missing/Partial scorecard before planning from this node."
tags: [task, testnet, deployment, multi-host, cloud, transport, ingress, staking, roadmap]
edges:
  - target: fact-mesh-verification-probe
    type: depends_on
    weight: 0.9
    note: "Phase 2's verdict instrument: exit code over eyeballed logs, and why logs cannot answer it"
  - target: fact-fanout-graph-density
    type: depends_on
    weight: 0.95
    note: "Sets the hard ceiling on fleet size: the send graph is a full mesh minus executor↔executor, and pruning never lowers the worst node"
  - target: fact-transport-loopback-bind-default
    type: depends_on
    weight: 0.9
    note: "The multi-host blocker is fixed but unproven across hosts; Phase 2 is the proof"
  - target: fact-testnet-generator
    type: depends_on
    weight: 0.9
    note: "Per-host placement and the two-pass (keys then addresses) flow are how a provisioner drives this"
  - target: fact-control-plane-peering
    type: related_to
    weight: 0.8
    note: "Peering discovers peers; it does not route for them — the distinction drives Phase 2"
  - target: fact-data-service
    type: related_to
    weight: 0.8
    note: "Chain state persists through it; validator stake does not — that asymmetry is Phase 3"
  - target: task-testnet-launcher
    type: preceded_by
    weight: 0.85
    note: "The launcher starts the fleet this roadmap then exercises; both consume manifest.json"
  - target: fact-test-suite-cloud
    type: related_to
    weight: 0.6
    note: "Baseline at rollout start: 1101/37/0, all single-host"
related: ["[[RNS transport: the production inter-node wire (RnsNetwork over rns-net, Resource transfer for large payloads)]]"]
source_url: "Empty"
---

# Multi-host testnet rollout: from a working local cluster to production

Written 10/05/2026. The scorecard below came from greps run 10/02–10/05/2026, not from
`CLAUDE.md`, which is flagged as predating the RNS/shielded/prover work — where the
two disagree, the code won. Confidence is 0.85 rather than 1.0 because those greps
were single-pass and absence-based: **re-verify before planning from this table.**

## Scorecard at rollout start

| Layer | State | Basis |
|---|---|---|
| Node→node peering | **Ready** | Register/ack/directory exchange over live UDP; 4-node full mesh verified end-to-end |
| Config & key generation | **Ready** | `testnet-gen`; boot verified through the real `Config::build()` |
| Multi-host addressing | **Fixed, unproven** | Composite loopback bind fixed; `Placement::PerHost` exists. **Never run on two machines** |
| Chain-state persistence | **Ready** | `data-service/src/store.rs` writes a state file; remote TCP reachable via `PNEUMATIC_DATA_ADDR` |
| Validator-stake persistence | **Missing** | `StakeStore` is an in-memory `DashMap` in `committer/src/epoch_manager.rs`; nothing writes it |
| Transaction ingress | **Missing** | No HTTP/RPC server found anywhere (`axum`/`actix`/`warp`/`hyper` server: no hits). `rest_api_version` is vestigial. No client binary |
| Slashing enforcement | **Missing** | `slash_fraction` appears only at `CostModel` construction sites; no application site |
| Transport at target size | **Unproven** | Dense mesh never run off loopback; relay (`transport_enabled: true`) never run at all |
| Observability | **Partial** | Per-node log files; **mesh fragments + `mesh-probe` landed 10/05/2026** (`fact-mesh-verification-probe`) — mesh state is now a signed, aggregate, CI-able artifact. Still no metrics endpoint, no alerting |

Runnable: `data-service`, `node-server` (composite, four roles), `committer`
(single role), `testnet-gen`. Proving is **client-side** — no worker crate calls a
prover — while proof *verification* is genuinely on the validation path
(`src/validation/shielded.rs`).

**The shape of it.** A cluster of correctly configured nodes will boot, find each
other, and run the pipeline. It cannot be pointed at by an outside user, and it
forgets its validator set when it restarts.

## Phase 1 — Transaction ingress and a client

Nothing downstream is measurable without it: no load testing, no epoch behavior
under traffic, no capacity number to size a fleet from.

Design constraint to respect from day one: a gateway reaches sentinels through the
same leaf links as every other node, so **gateways consume interfaces too** and
belong *inside* the topology rather than outside it. A gateway count that isn't in
`testnet-gen`'s plan will silently change the mesh.

**Exit test:** a script drives sustained transactions through a 4-node cluster and
they reach commit, with the committed block observable through the data service.

## Phase 2 — Transport viability off loopback *(do not defer)*

The riskiest unknown, because everything downstream assumes the mesh holds. Dense
mesh has never been exercised outside loopback, where there is no loss, jitter, or
MTU variance — a real network is worse, not better.

Sequence: **4 nodes on 4 real instances → prove formation and delivery → then
12–16 → then decide on relay.** Add loss/latency coverage locally (`tc`/`netem`) so
the failure is reproducible off the cloud.

**Instrument: `mesh-probe` landed 10/05/2026** (`fact-mesh-verification-probe`).
It judges a snapshot of node directories against the cluster's own
`manifest.json` and exits non-zero on any discrepancy, with `PARTIAL` and exit 1
when a host stayed silent. Two constraints it settled by checking, not assuming:
successful registrations **log nothing** (so no log-scraping health check can
work), and registration **runs a stake gate** (`registration.rs:434`), so the
probe must be a genesis-staked node or it is refused everywhere and looks like a
mesh that never formed. What is still missing is the **collector** that fills the
snapshot from live directory responses.

Current honest arithmetic (`fact-fanout-graph-density`): at 10 per role a sentinel
needs 39 direct links; role-graph pruning saves 5.8% at equal role counts and
**never lowers the worst node**. Levers that actually reduce it: fewer nodes per
role, or relay — which is untested code, not a known-working one.

**Exit test:** mesh formation and pipeline delivery sustained across real hosts,
then under injected loss. If this fails, the network shape changes (relay, fewer
validators, or different gateway placement) and every later phase is re-planned.

## Phase 3 — Persistence and restart correctness

Stake must survive a process. Today chain state outlives a restart via the data
service while the validator set does not — an asymmetry that will present as a
cluster that "comes back wrong".

Related unknown to close here: core still ships `StubStakingManager` and
`StubEpochReconciler`; the committer replaced the staking stub with a real
in-memory manager and has an epoch-reconcile handler, but **epoch behavior under
traffic is unverified**.

**Exit test:** kill a node and restart it; it rejoins with its identity, stake, and
epoch position intact, no manual steps, same validator set.

## Phase 4 — Enforcement, admission, and key custody

- **Enforce slashing.** A parameter with no enforcement site is not a security model.
- **Admission control and rate limiting** at the sentinel. `NodeTypeConfig` gives
  per-type connection bounds and a minimum stake; nothing stands between a client
  and a transaction flood.
- **Key custody.** Currently the operator holds every validator key as plaintext
  JSON generated locally. Acceptable for a testnet if deliberate; unacceptable for
  anything securing value. The alternative (per-node generation) needs a
  coordination store plus a config re-render, because bootstrap peers resolve at
  seed time.
- **A validator join path.** Adding a validator today means re-rendering every
  config and restarting the fleet.

**Exit test:** a byzantine validator is actually slashed; a flooded sentinel degrades
predictably; a new validator joins without a fleet-wide restart.

## Phase 5 — Operability

Metrics, structured logs, and an alert on **directories populated** — precisely the
thing that was invisible in every defect this session (wrong key, wrong port, wrong
bind all looked like a healthy node peering with nobody).

CI must stand up a **multi-host** cluster. The existing suite cannot detect a
loopback-only bind in either direction, because binding "any" is a superset of
binding loopback — a single-host CI will keep passing while a deployment is broken.

**Exit test:** from metrics alone, an operator can tell a healthy cluster from one
whose routes never went live.

## Phase 6 — Public testnet, then production

Incentivized testnet with an external API. Then independent review of the two
load-bearing, unaudited pieces: the **PQC binding-signature control plane** and the
**shielded circuit**.

## What I'd challenge

- **Don't treat "40 validators on one host" as a goal.** It was a useful constraint
  for building the generator and it hides single-host assumptions — three of which
  took a day to find.
- **Don't launch before Phase 1.** A cluster with no ingress has unknowable
  capacity, and a public testnet with no API is a cluster only you can use.

## First concrete action

The cheapest step that de-risks the most is the **4-node / 4-instance run**:
`testnet-gen --validators 4 --addresses-file <provisioner output>`, one
`data-service` sidecar per host seeded from the same `genesis.json`, the shared UDP
range opened between cluster members only, and `mesh-probe --manifest … --snapshot …`
as the verdict instead of eyeballing logs. Generation and judgment both exist;
**the remaining piece is the collector** — a genesis-staked node that peers,
requests directories, and writes the snapshot `mesh-probe` consumes. Note the
probe's green result means control-plane formation only; traffic still has to be
sent.
