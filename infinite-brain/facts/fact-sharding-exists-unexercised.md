---
id: fact-sharding-exists-unexercised
title: "Transaction sharding is already implemented and unit-tested — but has never run with shard_count > 1 across a network"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/07/2026: `deterministic_select_shard` (`src/epoch/leader.rs:168`) picks the executor set responsible for a transaction's shard from (shard_count, tx_id, epoch, prev_block_hash), and the sentinel *routes on it* — `processing.rs:150-156` sends a preload only to that shard's executors when `env_data.shard_count > 1` (`transaction_notifier.rs:46,66`). It is a **send-side optimisation with zero enforcement**: nothing on the receiving side knows what a shard is, and the envelope carries no shard coordinate. But every integration test, e2e harness, and the deployed `deploy/config/env/env.json:18` set `shard_count: 1`; only sentinel unit tests use 2. So the shard axis exists in consensus and one routing hop, and has never been carried across a real cluster. The rest of the pipeline (executor→finalizer→committer) still fans out to every node of a role."
auto_inject: true
applicable_when: "Any work on sharding, capacity, topology, committee selection, or the fan-out graph; or when asked how far this protocol scales"
confidence: 1.0
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If any e2e or deploy config sets shard_count > 1, if downstream stages (executor→finalizer→committer) gain shard-scoped routing, or if the quorum denominator becomes shard-scoped instead of global"
tags: [fact, sharding, capacity, topology, epoch, routing, fanout]
edges:
  - target: fact-fanout-graph-density
    type: related_to
    weight: 0.95
    note: "That fact's closing prediction — subsets thin the graph by themselves — is exactly what this code already does for one hop"
  - target: concept-executor-sharding
    type: refines
    weight: 0.9
    note: "The concept node describes the Shuffler abstractly and cites a file that no longer exists; this is the concrete call-site-level state"
  - target: concept-per-token-chains
    type: related_to
    weight: 0.8
    note: "Per-token chains make tokens the natural shard axis, but the envelope does NOT carry a token id — chain_id is write-only (see the Wire section)"
  - target: fact-observer-stake-paradox
    type: related_to
    weight: 0.75
    note: "A shard-scoped quorum needs a shard-scoped denominator; the same arithmetic that made observers expensive governs this"
related: ["[[The role fan-out graph is a full mesh minus executor↔executor]]", "[[Executor sharding per epoch (Shuffler)]]"]
source_url: "Empty"
---

# Transaction sharding is implemented, unit-tested, and has never been run

## What exists

Selection — `src/epoch/leader.rs:168`:

```rust
pub fn deterministic_select_shard(
    executors: &ExecutorSet, shard_count: u32, tx_id: &str,
    epoch_number: u64, prev_block_hash: &[u8],
) -> Option<Vec<Vec<u8>>>
```

Domain-separated (`SHARD_SHUFFLE_DOMAIN = 0x02`) and bound to the previous block hash, so the assignment is recomputable by any node without distribution — which is the property a sharded design actually needs, and it is already there. `shard_count == 1` short-circuits to the full sorted positive-stake set; zero-stake executors are excluded from both paths (Phase 6.6), so a slashed node is never listed as responsible.

Routing — `sentinel/src/sentinel/processing.rs:150-156`: when `shard_count > 1`, the sentinel calls `send_to_shard_executors_for_preload(tx, &shard_executors, …)`, which iterates the selected keys (`transaction_notifier.rs:66`) rather than fanning out to the Executor bucket. So the shard assignment is not decorative; it changes which peers receive the transaction.

**Correction (10/07/2026, second pass).** An earlier version of this node claimed the
wire already carried the axis because every `Message` bears `chain_id`. It does not,
and the claim was wrong in two ways, both verified:

- `chain_id` is **never read**. Outside its own definition and constructor the only
  hits are a test assertion and an e2e fixture. Inbound dispatch keys exclusively on
  the `action` string (`committer.rs:272`, `role_dispatcher.rs:138-156`).
- The two halves of the pipeline **disagree about what it means**. The sentinel passes
  `env.token_partition_id` — the constant string `"token"` for every transaction
  (`transaction_notifier.rs:34,54,97,129`) — while the executor, finalizer and
  committer pass `environment_id` (`executor.rs:858`, `message_dispatcher.rs:60`,
  `quoruming.rs:125`). It never distinguishes a token.

So there is **no routing coordinate on the envelope**: no shard id, no target role, no
sequence or nonce (replay protection is only the gossiper's TTL dedup). The token id
lives inside the signed body. Sharded clusters need a wire identity for a shard, and it
does not exist.

## What has never happened

| Where | shard_count |
|---|---|
| `deploy/config/env/env.json:18` | 1 |
| `node-server` e2e + helper configs | 1 |
| `committer/tests/pipeline_integration.rs:130` | 1 |
| `tests/transport_integration.rs:131,200` | 1 |
| sentinel unit tests (`helpers.rs:113`, `processing.rs:817`) | **2** |
| `src/epoch/tests/leader.rs:254` (selection only) | **2..4** |

Selection is well tested; the routing branch is tested only in sentinel unit tests; **no multi-node run has ever had more than one shard**, and `pneumatic_testnet_gen` never emits the key at all. Given this repo's track record — `transport_enabled` inert, `register_peer` as a test-only directory builder, the `RegisterAck` bucket-placement defect, directory responses never exercised until the peering initiator sent one — code with no live path is the place to expect defects.

## The consequence for scaling

Sharding is the only lever that reduces **work per node**; relay/gateways reduce only **links per node** while leaving message volume and per-message verification untouched. The current fan-out contract is "knows every node of role X", and the hybrid signature is 3796 B, so a round where every role talks to every peer costs on the order of N² hybrid verifications — which sharding cuts and relay does not.

But extending the axis is not free. The registry files peers by **role only** (five DashMaps keyed by public key), so nothing can answer "who is in shard 3", and a node selected into several shards needs the **union** of those shards' peers, which is what makes degree grow with shard membership rather than shrink. And `quoruming.rs` divides by the **global** `total_stake()`, so shard-scoped finalization requires a shard-scoped denominator — the same arithmetic that made a staked observer expensive governs this, and getting it wrong degrades fault tolerance silently rather than loudly.

## The cheap experiment before any of that

Set `shard_count: 2` in a generated cluster's `env.json` and boot it. Nothing needs to be written. The expected outcome is informative either way: the sentinel→executor hop should thin, the downstream stages should still fan out globally, and whatever *does* break is the actual list of work — instead of a design document written against code that has never executed.

## Second pass (10/07/2026): what else is missing, and two live defects

A read-only audit of every fan-out site (22 in the role crates, tabulated in
`raw/source-shard-routing-audit-20261007.md`) plus independent verification of each
claim. Confirmed: `committee` appears **zero** times in the codebase; every quorum
— block confirmation, finalizer count, finalizer stake, leader election, finalizer
assignment — divides by the **global** stake set.

**The executor pool is the whole validator set.** `StakeSet::to_executor_set()`
(`stake_sets.rs:32-36`) is literally `stakers.clone()`. So "sharding executors" is not
carving out a specialist class — it partitions every staked node, which is what makes
the union-of-shards degree problem unavoidable for anyone who stakes.

**`shard_quorum_percentage` is a dead knob with a lying comment**
(`environment.rs:161-163`): parsed, defaulted to 67.0, range-validated, and **read by
nobody** — its own doc says "Used by the signature collector when accumulating stake",
which is false. Shard-scoped quorum was clearly intended and never wired. Related: the
composite passes literal `66.6` and `4` into the Finalizer
(`node_server/plugins.rs:126-127`) while holding `env_data` in the same call, so even
the *global* quorum percentage is ignored in composite nodes.

**No receiver-side validation at all.** No executor, finalizer, or committer asks
"am I in the shard this is addressed to?" — a mis-routed message is processed normally.
Sharding today changes who you send to, never who may accept.

**Assignment computed, then thrown away.** `send_to_finalizer_for_preload`
(`transaction_notifier.rs:87-106`) and `send_sign_shielded` (`:119-138`) both take a
`finalizer_key: &[u8]` parameter and **ignore it**, broadcasting to the whole Finalizer
bucket. Only `FinalizerRequest` honours the deterministic finalizer selection.

### Defect: the shard selection salt is empty in production

Selection is seeded from `prev_block_hash` deliberately — `leader.rs:10-14` records the
reason: *"selection is only knowable once the previous block is actually mined (not
merely from the public epoch number + stake set)."* The sentinel obtains that salt at
`processing.rs:175-179` via `latest_block_hash(&self.env_data.environment_id)`, but the
provider implementation (`data.rs:301-307`) does:

```rust
let token_id = partition_id.as_bytes().to_vec();   // partition treated as a token id
let token = self.get_token(&token_id, partition_id)?;
```

so the lookup asks for a **token literally named `"env"`**. It does not exist, the
error collapses through `.unwrap_or_default().unwrap_or_default()`, and the salt becomes
the empty vector — on every call, not just at genesis, despite the comment claiming
otherwise ("unknown tip / I/O error → empty salt (genesis fails closed)").

**Blast radius, verified:** exactly two call sites, both sentinel-side — shard executor
selection (`processing.rs:177`) and finalizer selection (`finalizing.rs:164`). The
committer's leader election is a separate implementation that never calls this helper,
so it is unaffected. The audit's not-knowable-until-mined property is therefore not in
force for those two selections, which become derivable from `(domain, epoch_number,
tx_id)` alone.

**Why it stayed invisible: the trait's two implementations disagree about the
parameter.** `latest_block_hash(partition_id)` has three behaviours —

| Impl | Interpretation | Result for `environment_id` |
|---|---|---|
| trait default (`data.rs:66`) | ignores it | always `Ok(None)` |
| `DefaultDataProvider` (`data.rs:301`) | it is a **token id** *and* a partition | error → empty salt |
| `StubDataProvider` (`data.rs:838`) | it is a **partition key** | real tip if a token sits under it |

The stub's own comment says it *"Mirrors `DefaultDataProvider::latest_block_hash`"* — it
does not; it implements the other reading. Every existing test also stores no token, so
tests see an empty tip too and nothing looks wrong. The trap is armed rather than
sprung: the first test that stores a token under the environment id gets a real salt and
passes, while production still returns empty. Ambiguous parameter names plus a test
double that satisfies the other interpretation is how a silently-constant seed survives
an audit that was specifically about that seed.

### Defect: the one shard-aware send is the one send that reports nothing

`send_to_shard_executors_for_preload` (`transaction_notifier.rs:64-83`) spawns **one OS
thread and one fresh `current_thread` runtime per key per transaction**, discards every
send result (`let _ = conn.send(...)`), and returns `Ok(())` even when the key is absent
from the bucket. Because it calls `conn.send()` directly it also bypasses the
`(rhash, node_type)` delivery-failure counter that `send_to_all` maintains.

That last part matters for observability specifically: mesh fragments report
`delivery_failures`, so **a sharded cluster would look healthier than a dense one while
silently dropping shard-targeted preloads** — silence read as emptiness, in the exact
direction that hides the failure. Fix the accounting before using `shard_count > 1` as a
comparison baseline.

## The three things that turn "send fewer messages" into a shard boundary

Per-member delivery already works (the preload path proves it by key lookup), so the
registry filing dimension is *not* the expensive part. The three that are:

1. **Shard identity on the wire** — nothing on the envelope says what a message is for.
2. **Shard-scoped quorum denominators** — `shard_quorum_percentage` exists to express
   this and is dead; every denominator is global.
3. **Receiver-side validation** — the safety property that makes a shard boundary a
   boundary rather than a hint.
