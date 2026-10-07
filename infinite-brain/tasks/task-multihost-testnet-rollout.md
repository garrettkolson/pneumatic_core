---
id: task-multihost-testnet-rollout
title: "Multi-host testnet rollout: from a working local cluster to production"
type: task
namespace: pneumatic
visibility: namespace
summary: "OPEN — phased. A correctly configured cluster can boot and peer on one host; it cannot be driven from outside and forgets its validator set on restart. Seven phases with testable exits: (0) three correctness defects already on the live path, (1) transaction ingress, (2) transport viability off loopback, (3) durable stake, (4) enforcement and key custody, (5) operability, (6) public testnet and review, (7) shard boundaries — post-launch and capacity-gated. Phase 2 is the riskiest unknown and must not be deferred; Phase 0 is small and comes first because everything downstream measures through it."
auto_inject: true
applicable_when: "Planning or executing multi-host / cloud testnet deployment, deciding what to build next, or sizing a validator fleet"
confidence: 0.9
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "Any phase exit being met — especially an ingress surface existing, stake becoming durable, or the transport being exercised across real hosts. Re-verify the Missing/Partial scorecard before planning from this node."
tags: [task, testnet, deployment, multi-host, cloud, transport, ingress, staking, roadmap]
edges:
  - target: fact-mesh-verification-probe
    type: depends_on
    weight: 0.9
    note: "Phase 2's verdict instrument: exit code over eyeballed logs, and why logs cannot answer it"
  - target: fact-self-referential-quorum-denominator
    type: depends_on
    weight: 0.95
    note: "Phase 0 item 3 and the reason Phase 7's denominator work is 'declare and verify', not 'divide by shard stake'"
  - target: fact-sharding-exists-unexercised
    type: depends_on
    weight: 0.95
    note: "Phase 7 entry conditions, plus the two defects it surfaced that belong in Phase 0 instead"
  - target: concept-rns-transport
    type: depends_on
    weight: 0.9
    note: "Phase 2's relay decision depends on this: native RNS forwarding is not a config flag, it is unwired code"
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

Written 10/05/2026; revised 10/07/2026 after a routing and sharding audit that removed the
collector requirement, corrected the relay lever, added Phase 0 and Phase 7, and surfaced
two defects already on the live path. The scorecard below came from greps run 10/02–10/07/2026, not from
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
| Transport at target size | **Unproven** | Dense mesh never run off loopback. Relay is not a flag to flip: `rns-core`'s `TransportEngine` (paths, announces, tunnels, rate limits) is **constructed by nothing**, and `rns-net`'s `Transport` is a serial/TCP pipe to an RNode — so a gateway has to be an application-layer pneumatic node |
| Selection seeds (finalizer, shard) | **Defective** | The prev-block-hash salt collapses to empty in production (`data.rs:301` conflates partition with token id), defeating the property `leader.rs:10-14` records as its own reason for existing. Hits finalizer assignment **at `shard_count: 1`**, i.e. today |
| Observability | **Partial** | Per-node log files; **mesh fragments + `mesh-probe --fragments` landed 10/05/2026** (`fact-mesh-verification-probe`) — mesh state is now a signed, aggregated, CI-able artifact, and the `directory_observer` posture means the probe needs neither a collector nor stake. **Under-reports today**: per-key sends (`request_single_finalizer`) bypass `delivery_failures`, so a failed finalizer request is invisible to the probe |

Runnable: `data-service`, `node-server` (composite, four roles), `committer`
(single role), `testnet-gen`. Proving is **client-side** — no worker crate calls a
prover — while proof *verification* is genuinely on the validation path
(`src/validation/shielded.rs`).

**The shape of it.** A cluster of correctly configured nodes will boot, find each
other, and run the pipeline. It cannot be pointed at by an outside user, and it
forgets its validator set when it restarts.

## Phase 0 — Three things already on the live path *(small; do first)*

Found while scoping sharding on 10/07, and **neither is a sharding item** — both run in
the current `shard_count: 1` configuration, which is why they precede everything below.

**1. The selection salt is empty in production.** `assign_finalizer_deterministic`
(`sentinel/src/sentinel/finalizing.rs:163`) fetches the chain tip with
`latest_block_hash(&environment_id)`, and `DefaultDataProvider::latest_block_hash`
(`src/data.rs:301`) uses that string as a **token id** — so it looks up a token literally
named `"env"`, fails, and the failure collapses through
`.unwrap_or_default().unwrap_or_default()` into an empty salt. On every call, not just at
genesis, despite the comment claiming otherwise. `leader.rs:10-14` states why the salt
exists: selection should be knowable only *"once the previous block is actually mined
(not merely from the public epoch number + stake set)"*. That property is currently not
in force for finalizer assignment — or for shard selection, once it is enabled.

This is not pure plumbing. With **per-token chains there is no single env-level tip**,
which is almost certainly why the conflation happened in the first place.

Checked how each side of the protocol would source it, because they are different
processes with different access to chain state:

- **The sentinel already has the answer in hand.** `processing.rs:87-90` loads
  `get_token(&tx.token_id, token_partition_id)` for validation roughly 85 lines before the
  same function fetches the selection salt at `:175-179`. The tip of the chain the
  transaction actually extends is available with no new I/O, and it is canonically
  determined by the transaction itself — one token, so no tie-break rule to invent.
- **The committer already does this, and does it nondeterministically.**
  `epoching.rs:85-90` takes
  `self.tokens.iter().map(…last_hash_in).next().unwrap_or_default()` over
  `Arc<DashMap<Vec<u8>, Token>>` (`committer.rs:112`) — the tip of **whichever token
  DashMap iteration happens to yield first**. With more than one token cached, nothing pins
  the choice, and this is the seed for epoch **leader** election. Deterministic iteration is
  an explicit house rule elsewhere (the `C6` sort-before-select comments in
  `src/epoch/leader.rs`); this site predates it or missed it. Fix it under either option.
- The committer's chain state is deliberately **not persisted**
  (`epoching.rs:81-84`), so a sentinel that wanted an epoch-scoped salt would need new
  persisted state and a new read path, not just a new argument.

So the decision is per-token-tip versus epoch-state, with the recommendation being
per-token + **record the salt that was used**, so other nodes verify rather than recompute.
Both options re-key: per-token re-keys per block per token, epoch-state re-keys per epoch,
and the security difference is that re-key frequency rather than secrecy. Whichever wins,
the `.unwrap_or_default().unwrap_or_default()` pair goes — a genuinely missing tip is the
genesis case and must be explicit, not a collapsed error.

The Phase 0 exit test also got sharper: the salt must be non-empty **and identical across
nodes given the same fixture**, which is the property the leader site currently cannot
guarantee. It also needs a test that **stores a token**, because
`StubDataProvider` (`src/data.rs:838`) reads the same argument as a partition key and its
comment claims it mirrors `DefaultDataProvider` while implementing the other reading —
so the first well-intentioned test will pass while production keeps returning empty.

> **✅ Implemented 10/07/2026.** `latest_block_hash` is **deleted** from `DataProvider`
> rather than renamed — the single-string parameter that doubled as partition and token id
> was the root cause, and with the accessor gone the conflation is not expressible. The salt
> is now the tip of the token the transaction extends (`Sentinel::selection_tip`, and
> `chain_tip_of` where the token is already loaded), an unreadable token is an **error**
> rather than an empty salt, and the committer's leader seed goes through
> `canonical_chain_tip` (sorted-first token id) instead of DashMap iteration order.
> Genesis's placeholder token is still written, documented as vestigial. Decision recorded
> in `decision-selection-salt-per-token-tip`. Tests:
> `selection_tip_reads_the_transactions_own_token`,
> `selection_tip_refuses_a_missing_token`,
> `canonical_chain_tip_is_independent_of_insertion_order`; the two pre-existing
> tip-sensitivity tests now pass the salt explicitly, so they can only discriminate on it.
> **Still open:** recording the salt actually used, so a past committee can be re-derived —
> that is the same job as item 3 and touches the block/transaction surface.

**2. Per-key sends are not counted.** `request_single_finalizer`
(`sentinel/src/transaction_notifier.rs:214`, called from `finalizing.rs:128`) — like the
shard preload path — spawns a thread and a fresh runtime **per key per transaction**,
discards every send result, returns `Ok(())` even when the peer is missing from the
bucket, and calls `conn.send()` directly, bypassing the `(rhash, node_type)` failure
counter `send_to_all` maintains. Fragments report `delivery_failures`, so **a failing
finalizer request makes the mesh look healthier than it is** — silence read as emptiness,
in the direction that hides the fault. Phase 5 exists to prevent exactly this.

> **✅ Implemented 10/07/2026 — and it was hiding something worse than bad telemetry.**
> Both sites now go through a new `NodeRegistry::send_to_peers_blocking`, which applies the
> same `(rhash, node_type)` counter as `send_to_all` and **returns the targets that did not
> receive the payload** (a peer that is not in the bucket has no known rhash, so the counter
> cannot hold it — that is why the list is part of the API). One thread and one runtime now
> serve a whole batch instead of one per key per transaction. A shard preload that reaches
> nobody returns `NoTarget`; a single-finalizer request does the same, and
> `handle_rejection` no longer discards that result.
>
> Making the send report failure exposed a **live liveness defect**: the handler had been
> re-reading the transaction with `get_transaction`, which only serves the `Validated`
> state (`src/registry/pending.rs:246-257`), on an entry it had just moved to `Finalizing`.
> That lookup failed on **every** rejection, `if let Ok(tx)` swallowed it, and the send
> never ran — so a rejected transaction was reassigned in local state and delivered to
> nobody. Full account in `fact-finalizer-reassignment-never-delivered`. The transaction is
> now cloned from the lock scope that already holds it, and
> `handle_rejection_delivers_the_transaction_to_the_new_finalizer` asserts the recipient
> *receives bytes* (mutation-verified: skipping the send while returning `Ok(())` fails it).
> Two sibling silences fixed in the same pass: `send_to_all_blocking` scored a peer evicted
> mid-fan-out as a **successful** delivery, and a test fixture learned to assert
> `register_peer`'s return value instead of assuming capacity.
>
> **Still open here:** `register_peer` still reports refusal only as a `bool`, so a
> capacity-capped bucket can silently hold fewer peers than the code believes — the same
> class, one level down. Worth folding into Phase 2's honest-measurement work.

**3. The per-transaction quorum denominator is whoever showed up.**
`SignatureCollector::reconcile_signatures` (`finalizer/src/signature_collector.rs:161`)
computes its denominator as the **sum of the stake of signatures that arrived**, and if the
threshold is never reached it falls through to `candidates.first()` and proceeds. Nothing
compares the voter set to the executors the sentinel assigned. `fact-self-referential-quorum-denominator`
has the full analysis; the part that belongs in this roadmap is the ordering consequence:

At `shard_count: 1` the assigned set is every positive-stake executor, so in normal
operation arrivals equal the validator set and the arithmetic is accidentally right. Item 2
above is what breaks the accident — **an uncounted dropped send lowers the threshold with
it**, silently, with no metric and no error. So items 2 and 3 are one problem wearing two
hats, and Phase 7 cannot start until the denominator names a *set* rather than a
*happening*.

The minimal Phase 0 version is not a redesign: pass the assigned executor set into
reconciliation, and on a shortfall either reject or at minimum **count and log** it. The
block already records global `total_stake`/`total_voters` it never enforced
(`finalizer/finalizing.rs:108-115`), so the declared set is already available to compare
against.

**Exit test:** a test that stores a token observes a non-empty salt through the *production*
provider, not only the stub; a send to a peer removed from a bucket increments a visible
counter; and a transaction whose votes fall short of the assigned set produces a
*distinguishable outcome* — a rejection or a counter — rather than a finalized block. All
three are small. None is optional, because Phases 1–2 consist of measuring through code
that currently reports success when it did not succeed, and finalizes on whoever answered.

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

**Instrument: `mesh-probe --fragments`** (`fact-mesh-verification-probe`, updated
10/07). Each node signs a self-report of its own directories, the operator log pipeline
ships the fragments, and the probe aggregates them against the cluster's own
`manifest.json`, exiting non-zero on any discrepancy and printing `PARTIAL` when a host
stayed silent. The design moved twice since this node was written, both times toward less
machinery: **there is no collector** — a collector needed stake to register at all
(`fact-observer-stake-paradox`) — and **the probe no longer needs to be a staked node**
(`directory_observer`, 10/07). Successful registrations still log nothing, which remains
the whole reason the artifact exists.

Current honest arithmetic (`fact-fanout-graph-density`): at 10 per role a sentinel
needs 39 direct links; role-graph pruning saves 5.8% at equal role counts and
**never lowers the worst node**.

Two corrections to how that lever was described here on 10/05:

- **Relay is not a switch.** `rns-core::transport::TransportEngine` implements path
  tables, announce handling, tunnels, blackholes and rate limiting, and **nothing in the
  crate constructs it**; `rns-net`'s `Transport` is a serial/TCP pipe to an RNode device.
  So the Phase 2 choice is between *fewer validators*, *denser host packing*, and an
  *application-layer gateway* — a pneumatic node holding many links and forwarding
  envelopes byte-for-byte. The last needs a hop limit on the frame rather than the signed
  message, and it needs Phase 4's rate limiting before it is safe to run: a ~200-byte
  request can pull a ~156 KB directory answer, so an unmetered relay is a reflection
  amplifier, not infrastructure.
- **We have not established which resource actually binds.** The 39-interface figure
  counts sockets and threads, and ~40 of either is unremarkable on a VM. With a
  3796-byte hybrid signature, every message from every peer needs a verification, so a
  dense round is plausibly **O(N²) verifications before it is socket-bound — and gateways
  do nothing about that.** Phase 2 should record, per node at target size: verifications/s,
  threads, RSS, sockets. Without those numbers, choosing between a gateway and a shard is
  a preference, not an inference.

**Exit test:** mesh formation and pipeline delivery sustained across real hosts,
then under injected loss, **with the four per-node resource numbers recorded**. If this
fails the network shape changes — an application-layer gateway, fewer validators, or
different host packing — and every later phase is re-planned, with the choice made from
the resource numbers rather than from taste.

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
  per-type connection bounds and a minimum stake; nothing stands between a client and a
  transaction flood. **This is also the precondition for any gateway**: there is no rate
  limiting anywhere in `src/` today, and an unmetered forwarder is an amplifier.
- **Delete `shard_quorum_percentage` or make it mean something.** It is parsed, defaulted
  to 67.0, range-validated, and **read by nobody**, while its own doc comment claims the
  signature collector uses it (`environment.rs:161-163`). A knob that silently does nothing
  is worse than a missing one: an operator who sets it believes shards have their own
  quorum. The composite is a near cousin — it passes literal `66.6` and `4` into
  `Finalizer::new` while holding `env_data` in the same call
  (`node_server/plugins.rs:126-127`). Checked rather than assumed: the `66.6` **is** used,
  so `env_data.quorum_percentage` really is ignored on the finalize path; the `4` feeds
  `total_voters`, whose only consumer `check_quorum` has no production caller, so it is
  inert rather than dangerous. Fix both, but they are not the same severity.
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

One more prerequisite from the 10/07 audit: metrics and fragments need a **dimension**
before Phase 7. Failures are keyed `(rhash, NodeRegistryType)` and fragments bucket by
role, so nothing can distinguish "shard 3 is down" from "the mesh is fine". Add the
dimension when the shard exists, but do not let the keying harden further in the
meantime.

CI must stand up a **multi-host** cluster. The existing suite cannot detect a
loopback-only bind in either direction, because binding "any" is a superset of
binding loopback — a single-host CI will keep passing while a deployment is broken.

**Exit test:** from metrics alone, an operator can tell a healthy cluster from one
whose routes never went live.

## Phase 6 — Public testnet, then production

Incentivized testnet with an external API. Then independent review of the two
load-bearing, unaudited pieces: the **PQC binding-signature control plane** and the
**shielded circuit**.

## Phase 7 — Shard boundaries *(post-launch, capacity-gated)*

Transaction sharding already exists — `deterministic_select_shard`, plus a sentinel that
routes preloads only to the selected executors — and **has never run with
`shard_count > 1` across a network** (`fact-sharding-exists-unexercised`). That proximity
is tempting and it is a trap: the existing code thins *one send*; it does not create a
shard. Do not open this phase because the code is close. Open it because a number
demands it.

**Entry gate — all three, or leave it closed:**
1. Phases 1 and 2 produced a measured capacity number, plus Phase 2 resource profile.
2. The fleet is at or past the point where those numbers fail. Not before.
3. A written answer to the committee-shape question below — the two live options, with
   their costs, are in this section; picking one is the phase's first deliverable.

**Why the existing sharding does not relieve density.** `StakeSet::to_executor_set()` is
literally `stakers.clone()` — the executor pool *is* the whole validator set. Shards are
therefore drawn *per transaction* from everyone, so any staked node lands in effectively
every shard over time and needs the **union** of their peers. Density falls only if
committee membership is *disjoint*: stakers assigned to a shard rather than sampled from
all stakers per transaction. That is a staking and consensus change, and it is the actual
subject of this phase. The routing was the easy part.

**The committee-shape choice.** "Validator-set shards" is not one design, and the
protocol's shape rules out the naive one. Two viable answers:

- **Rotating disjoint committees.** Assign each staker to a shard deterministically from
  (sorted stake set, epoch number, domain seed) — disjoint membership, and still
  *recomputable*, which keeps the property that makes the current design attractive: no one
  has to distribute assignments. Rotation per epoch prevents long-lived shard capture, and
  degree falls to roughly `N / shard_count`. Costs: the shard a block belongs to becomes a
  consensus fact the block must name; a committee-size floor is mandatory (three validators
  is a target, not a committee); and every node must know its own membership.
- **Token-scoped committees.** Per-token chains (`Token.blockchain`) already partition
  *state* along a token axis, so a committee per token — or per token group — gets disjoint
  membership and a state partition that follows the same line. This is the axis the
  protocol is actually shaped for, and `chain_id` was evidently meant to carry it before it
  became a write-only field. Costs: risk concentrates on high-value tokens with no work
  rebalancing, a hot token is a honeypot with a fixed committee, and it requires
  `chain_id` to become a real, signed, *read* routing field — a wire change, so lockstep.

Sampling sharding (what exists) is a third option and it is not nothing: it reduces
per-transaction execution work by roughly `1 / shard_count` without touching membership. It
just never reduces links, and its tail is unsharded.

**The three things that turn "send fewer messages" into a boundary:**

1. **Shard identity on the wire.** The envelope carries none — no shard id, no target
   role, no sequence or nonce. `chain_id` is write-only, and the two halves of the
   pipeline disagree about what it means (the sentinel sends the constant `"token"`,
   everyone else sends `environment_id`). A wire change means a **lockstep upgrade**: rmp
   encodes structs positionally, so frames are not byte-compatible across it.
2. **A real denominator, then a shard-scoped one.** Not "divide by the shard's stake" —
   see Phase 0 item 3. The per-transaction denominator is currently the stake that arrived
   (`fact-self-referential-quorum-denominator`), so there is nothing to scope yet. The block
   must name the responsible set, the receiver must verify against it, and only then can
   "shard quorum" mean anything. The committer's block-confirmation gate
   (`quoruming.rs:55-75`) *does* have a declared global denominator — that is the gate to
   learn the shape from, and the one that keeps fault-tolerance claims honest today.
3. **Receiver-side validation.** Nothing on the receiving side asks "is this mine?" — a
   mis-routed message is processed normally. Without this, a shard is a routing hint, not
   a safety property.

**Exit test:** a cluster with `shard_count > 1` where a message addressed to another shard
is *rejected and counted*, quorum is computed against the shard's own stake set, and
mesh-probe reports per-shard health — with the Phase 0 send accounting in place so none of
the above can be silently absent.

## What I'd challenge

- **Don't treat "40 validators on one host" as a goal.** It was a useful constraint
  for building the generator and it hides single-host assumptions — three of which
  took a day to find.
- **Don't launch before Phase 1.** A cluster with no ingress has unknowable
  capacity, and a public testnet with no API is a cluster only you can use.
- **Don't start Phase 7 because the code is already there.** The existing shard path is a
  send-side optimisation with zero enforcement and no receiver validation; running it
  wider adds surface without adding a boundary. Its two genuine defects are Phase 0, and
  they are worth fixing whether or not sharding ever ships.
- **Don't assume sharding buys density.** With the executor pool equal to the whole stake
  set, it does not. The real levers are disjoint committees (protocol) or gateway
  placement (deployment), and they are different decisions with different costs.

## First concrete action

The cheapest step that de-risks the most is the **4-node / 4-instance run**:
`testnet-gen --validators 4 --addresses-file <provisioner output>`, one
`data-service` sidecar per host seeded from the same `genesis.json`, the shared UDP
range opened between cluster members only, and
`mesh-probe --manifest … --fragments <dir>` as the verdict instead of eyeballing logs.

**Simpler than this node claimed on 10/05.** The collector it told you to build is no
longer required. Nodes sign and dump their own fragments (`mesh_fragment_path`, emitted
by the generator), the operator log shipper moves the files, and the probe aggregates.
Nothing needs stake and nothing needs a link to every node. So the remaining piece is
genuinely the run itself — plus, in parallel because it is small, the Phase 0 salt and
counter fixes, since every number this run produces otherwise comes from code that
reports success when it did not succeed.

A green probe still means **control-plane formation only**. Traffic has to be sent; that
is Phase 1, and it is why ingress precedes every capacity claim.
