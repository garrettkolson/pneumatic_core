---
id: task-multihost-testnet-rollout
title: "Multi-host testnet rollout: from a working local cluster to production"
type: task
namespace: pneumatic
visibility: namespace
summary: "PHASED, IN PROGRESS. **Phase 0 closed 10/07/2026** (per-token selection salt + production-provider exit test, per-key send accounting, quorum measured against a declared set with the committer gate fixed). A correctly configured cluster can boot and peer on one host; it cannot be driven from outside. **Read the Live scorecard and Pickup cards near the top of this node before anything else** — they carry current status per phase, what is already in place, the first thing to do, and the trap each phase has. Remaining: (1) transaction ingress — nothing exists, do this next; (2) transport off loopback — instrumented but never run, no code needed to start; (3) durable staking ops; (4) rate limiting and key custody; (5) operability metrics; (6) public testnet; (7) shard boundaries, capacity-gated and deliberately refused in code. Phase 2 is the riskiest unknown and must not be deferred."
auto_inject: true
applicable_when: "Planning or executing multi-host / cloud testnet deployment, deciding what to build next, or sizing a validator fleet"
confidence: 0.9
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If any Pickup card states a status the tree contradicts; if a phase is started out of order; if a line anchor in this node no longer points at what it cites; if Phase 0's mutation-verified tests are edited without re-running the mutations" "Any phase exit being met — especially an ingress surface existing, stake becoming durable, or the transport being exercised across real hosts. Re-verify the Missing/Partial scorecard before planning from this node."
tags: [task, testnet, deployment, multi-host, cloud, transport, ingress, staking, roadmap]
edges:
  - target: fact-mesh-verification-probe
    type: depends_on
    weight: 0.9
    note: "Phase 2's verdict instrument: exit code over eyeballed logs, and why logs cannot answer it"
  - target: playbook-picking-up-a-roadmap-phase
    type: depends_on
    weight: 0.9
    note: "The procedure and repo-specific traps for starting any phase cold — read it before the pickup cards"
  - target: fact-committer-confirmation-gate-did-not-gate
    type: related_to
    weight: 0.95
    note: "What auditing the actually-live gate found instead of the dead function the roadmap named — three silent failures behind the only quorum an ordinary transaction passes"
  - target: fact-self-referential-quorum-denominator
    type: depends_on
    weight: 0.95
    note: "Phase 0 item 3 (done 10/07: the denominator is declared and a shortfall fails) and the reason Phase 7's denominator work is 'declare and verify', not 'divide by shard stake'"
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

---

## Live scorecard — start here *(updated 10/07/2026, supersedes the table above)*

Every row was checked against the working tree today rather than remembered. Where it
contradicts the rollout-start table, the contradiction is named so that table stays
useful for its reasoning and stop being used for its state.

| Phase | Status | Already in place | First thing to do |
|---|---|---|---|
| **0 — live-path defects** | **✅ closed 10/07** | ADR-019 per-token salt; per-key send accounting; declared `ResponsibleSet`; committer confirmation gate verified | Nothing. If you touch quorum, finality or selection, re-run the mutation checks in `playbook-picking-up-a-roadmap-phase` first |
| **1 — ingress** | **○ not started** | No *transaction* ingress: no client binary, no RPC, `rest_api_version` vestigial. Note there *is* one HTTP responder in the tree — the health/metrics server in `src/telemetry.rs`, raw tokio, no HTTP crate — which is the shape a first ingress endpoint would reuse | Decide the ingress surface (see the pickup card) *before* writing a handler; then one transaction from a client to a committed block, observable in the data service |
| **2 — transport off loopback** | **◐ instrumented, unrun** | Loopback-bind fix, `Placement::PerHost`, signed mesh fragments, `mesh-probe --fragments`, `directory_observer` posture (probe needs no stake), send accounting so failures reach the probe | Book four instances and run it. No code first |
| **3 — persistence & restart** | **◐ partial** | Chain state + stake/executor snapshots persist: `advance_epoch_to` writes both (`committer/src/committer/epoching.rs:156-171`) and boot loads one fail-closed | `StubStakingManager` is still a stub: decide what staking ops must survive a restart, then make one of them survive one |
| **4 — enforcement, admission, custody** | **◐ partly landed** | Slashing **is** enforced: two sites — an invalid chain's tip proposer and double-sign resolution (`committer/src/epoch_manager.rs:205-227`, `:262-275`) — applied through a real `StakingManager` (`:111-124`) and persisted with the epoch snapshot. Config hygiene items done | Rate limiting at the sentinel — nothing exists in `src/` and it gates any gateway |
| **5 — operability** | **◐ plumbing exists** | `src/telemetry.rs`: Prometheus text-format metrics on lock-free atomics, health endpoint over raw tokio, tracing init. Mesh fragments | Decide what "directories populated" looks like as a metric and alert on it — the plumbing is not the gap |
| **6 — public testnet** | **○ blocked on 1 & 2** | `testnet-gen` (keys, configs, env, genesis, topology, fragments); no `up/down/status` launcher (`task-testnet-launcher`) | Do not start here. Phase 1 first |
| **7 — shard boundaries** | **⛔ gated, deliberately** | Sharding exists and is unit-tested; `Finalizer::responsible_set()` refuses `shard_count > 1` rather than guessing | Nothing, until a measured capacity number fails |

**Four corrections to the rollout-start table**, so nobody re-derives them:
- *Selection seeds "Defective"* — fixed 10/07 by ADR-019; the empty-salt path is gone at
  the type level (`latest_block_hash` deleted) and the behaviour is pinned through the
  **production** provider (`selection_salt_through_the_production_data_provider`).
- *Validator-stake persistence "Missing… nothing writes it"* — **too strong**. Epoch
  stake and executor snapshots are written on epoch advance and a failure aborts the
  advance rather than swallowing it. What is genuinely missing is staking-operation
  persistence (`StubStakingManager`) and identity/peer rejoin.
- *Slashing enforcement "Missing… no application site"* — **wrong**, and it would have
  sent someone to build what already exists. `slash_fraction` has two application sites:
  an invalid chain's tip proposer (`committer/src/epoch_manager.rs:205-227`, where the
  audit-driven fix gave `misshapen_tokens` an economic effect instead of leaving it a dead
  accumulator) and double-sign resolution (`:262-275`). Ops are applied exactly once per
  op by a real `StakingManager::apply_ops` (`:111-124`, which also records that an earlier
  version deducted twice), and the result rides the epoch snapshot into the data service.
  What Phase 4 actually lacks is the **network** half: a byzantine validator run as a second
  committer process, slashed, with the reduced stake visible in the persisted snapshot.
- *Observability "under-reports"* — the per-key send path now carries the same
  `delivery_failures` accounting as `send_to_all`, so a dropped finalizer request is
  counted. Residual: `register_peer` still signals capacity refusal as a bare `bool`.

## Pickup cards

One per open phase: what to read, what the first commit looks like, and the trap that
specific phase has. Phases 1 and 2 are the only two worth starting today; the rest are
recorded so they are not started out of order.

### Phase 1 — ingress
- **Read first:** this section, `fact-control-plane-silent-drop-paths` (what "it looked
  healthy" means here), `task-testnet-launcher` (the generator side is done; the runbook
  half is not).
- **First commit:** the *decision*, not the server — which surface (raw tokio HTTP like
  `telemetry.rs`, or a framed MsgPack endpoint on the same length-prefixed protocol the
  nodes already speak), and whether the ingress node is a sentinel or a new role. The
  wire already has no client-facing message type; inventing one is a wire change, and
  rmp encodes positionally, so it is lockstep with everything.
- **Then:** one transaction, one client, one 4-node cluster, assert the committed block
  is readable through the data service — the same shape as
  `selection_salt_through_the_production_data_provider`: assert the *effect* through the
  production path, never through a stub.
- **Trap:** the gateway question is a Phase 2 topology question wearing an ingress hat.
  Gateways consume leaf links; if they are not in `testnet-gen`'s plan they silently
  change the mesh.

### Phase 2 — transport
- **Read first:** `fact-mesh-verification-probe`, `fact-fanout-graph-density`,
  `fact-transport-loopback-bind-default`.
- **First commit:** none. Book four instances, run
  `testnet-gen --validators 4 --addresses-file …`, one `data-service` sidecar per host
  from the same `genesis.json`, open the shared UDP range between members only, and let
  `mesh-probe --manifest … --fragments <dir>` be the verdict.
- **Trap #1:** a green probe means **control-plane formation only**. Send traffic.
- **Trap #2:** the exit test requires four *numbers* per node at target size
  (verifications/s, threads, RSS, sockets). Without them the gateway-vs-shard decision is
  a preference. A dense round is plausibly O(N²) signature verifications before it is
  socket-bound, and gateways do nothing about that.
- **Trap #3:** CI cannot detect a loopback-only bind in either direction, because binding
  "any" is a superset of binding loopback. A single-host CI passes forever.

### Phase 3 — persistence
- **Read first:** `tests/data_service_boot.rs` (the two boot reads that gate a node),
  `committer/src/committer/epoching.rs:156-171`.
- **First commit:** kill a node mid-epoch and restart it, and write down what actually
  differs afterwards — before changing anything. The roadmap's claim is "it comes back
  wrong"; nobody has recorded *how*.
- **Trap:** `register_peer` is capacity-capped (`get_max_node_number`) and reports refusal
  only as a `bool`. A restarted node that "rejoined" may have a directory that is quietly
  short. Count it before trusting the rejoin.

### Phase 4 — enforcement
- **First commit:** rate limiting at the sentinel, because it gates the gateway and every
  later network shape. Admission control already has per-type bounds and a stake floor in
  `NodeTypeConfig`; the missing half is per-source rate.
- **Trap:** slashing exists in-process, so it *looks* done. The exit test is "a byzantine
  validator is actually slashed" **across a network** — a conflicting block produced by a
  second committer process, not a unit fixture.

### Phase 5 — operability
- **First commit:** one metric + one alert for directories populated, then delete the
  habit of reading logs to answer a mesh question.
- **Trap:** failures are keyed `(rhash, NodeRegistryType)` and fragments bucket by role.
  Add the shard dimension when Phase 7 needs it, but stop hardening the keying before
  then — otherwise "shard 3 is down" becomes permanently indistinguishable from "the mesh
  is fine".

### Phase 7 — shards
- **Entry gate is three conditions, all required** (measured capacity, the fleet is at
  the point where it fails, and a written committee-shape choice). "The code is nearly
  there" is not one of them; the existing path thins one send and validates nothing on
  receipt.
- **First commit:** the selection record (epoch, salt, committee hash) on the
  sentinel→finalizer message — `Finalizer::responsible_set()` already refuses
  `shard_count > 1`, so the phase opens with a compile-time-shaped hole rather than a
  silent wrong answer. That refusal is the seam to work from.
- **Trap:** the envelope has no shard identity, and `chain_id` is write-only — the
  sentinel sends the constant `"token"` while every other role sends
  `environment_id`. Any of this is a lockstep wire change.

## If you are picking this roadmap up cold

Read `playbook-picking-up-a-roadmap-phase` first. It is the procedure and the set of
repo-specific traps (the test suite exceeds the default command timeout; absence-grep
findings that miss multi-line call sites; the mutation-verify standard every "was
rejected" test here is held to). Line numbers in this node **drift** — the anchors were
true on 10/07/2026; verify each before acting on it.

## Phase 0 — Three things already on the live path ✅ **CLOSED 10/07/2026**

> Kept in full because the reasoning is the point: two of the three were found by
> auditing a *sharding* question and both ran at `shard_count: 1`. Read the item
> write-ups before changing selection, send accounting, or quorum — each has a
> mutation-verified test that the change will break.

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

**3. ✅ DONE 10/07 — the quorum denominator is now a declared set.**
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

> **What landed.** `reconcile_signatures(tx_id, &ResponsibleSet)` — the denominator is the
> set's total, each vote is priced by the set rather than by the `current_stake` stamped on
> the vote, votes from unassigned keys are excluded **and counted**, and a shortfall is an
> `Err` plus `quorum_shortfall_count()` instead of a fallthrough to `candidates.first()`.
> `Finalizer::responsible_set()` resolves the set from the epoch stake snapshot and
> **refuses** at `shard_count > 1`: a shard is chosen from the per-transaction selection
> salt, which the finalizer is never given, so the global set there would fail every
> sharded transaction while looking like a working check. Dead count-based machinery went
> with it — `check_quorum` and the `total_voters` constructor parameter are deleted (7
> call sites), the composite now passes `env_data.quorum_percentage` instead of a literal
> `66.6`, and `shard_quorum_percentage` is removed from `EnvironmentMetadata`, the spec,
> the deploy config and every fixture.
>
> **The roadmap pointed at a dead function.** `try_finalize` has no caller: the live
> standard path is `try_finalize_optimistic`, which per ADR-005/ADR-010 waits for no
> quorum and reconciles no signatures, so the self-referential denominator was live only on
> the shielded path. Auditing the *actually live* gate instead turned up three worse
> defects, now fixed — a committer obeyed a `BlockQuorumReached` claim without recomputing
> it, from any registered role; a committer never counted its own vote, so three equal
> committers could never reach 67% and nothing would ever become `Confirmed`; and votes
> arriving before their stake set were discarded silently. See
> `fact-committer-confirmation-gate-did-not-gate`.
>
> **Still open here:** the *assigned* set is only exact at `shard_count: 1`, where
> selection returns every positive-stake executor. Carrying a selection record (epoch,
> salt, committee hash) on the sentinel→finalizer message is Phase 7's job and is
> deliberately not approximated here.

**Exit test:** a test that stores a token observes a non-empty salt through the *production*
provider, not only the stub; a send to a peer removed from a bucket increments a visible
counter; and a transaction whose votes fall short of the assigned set produces a
*distinguishable outcome* — a rejection or a counter — rather than a finalized block. All
three are small. None is optional, because Phases 1–2 consist of measuring through code
that currently reports success when it did not succeed, and finalizes on whoever answered.

> **Status of the three exit tests.** (2) is satisfied — `register_peer` refuses an
> over-capacity bucket and the refusal is observable, with the residual named above. (3) is
> satisfied and then some: `one_vote_is_no_longer_a_quorum`,
> `a_shortfall_refuses_instead_of_lowering_the_bar`, and the committer-side
> `quorum_claim_is_refused_unless_this_node_computes_quorum_too` all assert a distinguishable
> outcome, mutation-verified. **(1) is not satisfied**: the salt tests drive
> `StubDataProvider`, so "the production provider returns a non-empty salt" is still an
> assumption rather than an observed fact — which is precisely the class of gap Phase 0
> exists to close. It needs a `DefaultDataProvider`-against-a-real-data-service test (or a
> service-backed test fixture) before Phase 0 can be called complete.

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

- **✅ Mostly done 10/07 — slashing does have enforcement sites.** The roadmap claimed
  otherwise; it does not (see the correction above: invalid-chain proposer and double-sign,
  applied through a real `StakingManager` and persisted on epoch advance). **Remaining:** the
  exit test's version of the claim — slash a byzantine validator running as a *separate*
  committer process and see the reduced stake in the snapshot the other roles read. In-process
  fixtures prove the arithmetic, not the protocol.
- **Admission control and rate limiting** at the sentinel. `NodeTypeConfig` gives
  per-type connection bounds and a minimum stake; nothing stands between a client and a
  transaction flood. **This is also the precondition for any gateway**: there is no rate
  limiting anywhere in `src/` today, and an unmetered forwarder is an amplifier.
- **✅ DONE 10/07 — config knobs that lied.** `shard_quorum_percentage` is **deleted**
  (struct field, spec, boot validation, deploy `env.json`, every fixture) rather than wired:
  it was parsed, defaulted to 67.0, range-validated, read by nobody, and its doc comment
  claimed the signature collector used it. A knob that silently does nothing is worse than a
  missing one — an operator who sets it believes shards have their own quorum. Restore it in
  Phase 7 *only if* shard-level quorum is implemented. The composite's literals went the same
  way: it passed `66.6` and `4` into `Finalizer::new` while holding `env_data`; it now passes
  `env_data.quorum_percentage`, and `total_voters` is gone with the dead `check_quorum` that
  was its only consumer. The severity difference between those two literals was established
  by reading, not assuming — `66.6` was live, `4` was inert.
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
   see Phase 0 item 3. The denominator is now a **declared set**
   (`fact-self-referential-quorum-denominator`), but at `shard_count: 1` only:
   `Finalizer::responsible_set()` refuses above one shard because it cannot know the salt,
   so the first Phase 7 deliverable is the selection record that tells it. The block
   must name the responsible set, the receiver must verify against it, and only then can
   "shard quorum" mean anything. The committer's block-confirmation gate
   (`quoruming.rs`) *does* have a declared global denominator — and now verifies its own
   numerator as well (`fact-committer-confirmation-gate-did-not-gate`) — that is the gate to
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
Nothing needs stake and nothing needs a link to every node.

**So the remaining piece is genuinely the run itself.** The caveat this section carried —
"run it in parallel with the Phase 0 fixes, because every number comes from code that
reports success when it did not succeed" — is **gone as of 10/07/2026**: all three Phase 0
items are closed, the salt is read through the production provider in a test, per-key sends
are accounted, and the quorum gate verifies its own arithmetic. A 4-node run's numbers can
be trusted now. Nothing stands between this action and booking four instances.

A green probe still means **control-plane formation only**. Traffic has to be sent; that
is Phase 1, and it is why ingress precedes every capacity claim.
