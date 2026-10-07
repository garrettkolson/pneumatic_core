# Sharding & message-routing map — pneumatic_core

Scope: root `pneumatic_core` + `sentinel`, `executor`, `finalizer`, `committer`, `node-server`, `data-service`, `testnet-gen`, `prover`. Read-only; no files modified.

**Headline:** one shard concept exists and is real (`deterministic_select_shard`, executor-only), and it is wired into exactly **one** pipeline hop (sentinel → executor preload). It is dead at runtime today: every shipped config sets `shard_count: 1`, which takes the `else` broadcast branch. Everything else — shard-scoped quorum, shard-scoped leader, shard-scoped routing coordinates on the wire, shard-dimension registry filing — does not exist.

---

## 1. What "shard" means in this codebase today

### 1a. The one real concept: executor shards (exists, implemented, unit-tested, unreachable in production configs)

**Definition — `src/epoch/leader.rs:168-242`:**

```rust
pub fn deterministic_select_shard(
    executors: &ExecutorSet,
    shard_count: u32,
    tx_id: &str,
    epoch_number: u64,
    prev_block_hash: &[u8],
) -> Option<Vec<Vec<u8>>> {
```

The body: (i) `shard_count == 1` → shortcut returning *all* positive-stake executors sorted (`leader.rs:181-194`); (ii) a shard index from a domain-separated seed (`leader.rs:198-200`):

```rust
let seed = derive_selection_seed(SHARD_INDEX_DOMAIN, epoch_number, prev_block_hash, tx_id.as_bytes());
let mut shard_rng = StdRng::from_seed(seed);
let shard_index: u32 = shard_rng.gen_range(0..shard_count);
```

(iii) a deterministic Fisher-Yates shuffle of the executor keys (`leader.rs:203-204`, `Shuffler::new` at `leader.rs:70-99`, seeded with `SHARD_SHUFFLE_DOMAIN`), (iv) zero-stake executors dropped (`leader.rs:212-218`), (v) **stake-balanced greedy assignment** — each executor goes to the currently-lowest-stake shard (`leader.rs:222-234`):

```rust
let target_shard = (0..shard_count as usize)
    .min_by_key(|&i| shard_stakes[i])
    .unwrap_or(0);
```

Domain bytes at `src/epoch/leader.rs:18-21` (`LEADER_DOMAIN=0x01`, `SHARD_SHUFFLE_DOMAIN=0x02`, `FINALIZER_DOMAIN=0x03`, `SHARD_INDEX_DOMAIN=0x04`). Seed shape at `leader.rs:29-44`.

**The pool — `src/epoch/stake_sets.rs:68-75`:**

```rust
/// Maps executor public keys to their stakes. Used for deterministic shard
/// assignment: the sentinel computes `f(tx_id, epoch, shard_count) → shard`
/// then routes the transaction only to executors in that shard.
pub struct ExecutorSet {
    pub executors: HashMap<Vec<u8>, u64>,
}
```

`ExecutorSet::shuffler()` at `stake_sets.rs:105-112` (sorts keys before shuffling — C6 determinism guard). `StakeSet::to_executor_set()` at `stake_sets.rs:30-36` is a plain `stakers.clone()` — **the executor pool is simply the whole stake set**, no separate staking/registration path.

**Where it is consumed — exactly one production call site:**

- `sentinel/src/sentinel/processing.rs:181` inside `get_shard_executors` (`processing.rs:167-195`), gated by `processing.rs:152`: `if self.env_data.shard_count > 1 {`
- Fed from `executor_set_cache` (`sentinel/src/sentinel.rs:45`, wired at `sentinel.rs:88-92` to `DataProvider::get_executor_set`).
- Persisted at epoch boundary by the committer: `committer/src/committer/epoching.rs:143-148` `save_executor_set(new_epoch_number, stake_set.to_executor_set(), &token_partition_id)`.
- Data-plane plumbing exists and is real: `src/data.rs:54,57` (trait), `:281,292` (`DefaultDataProvider`), `ExecutorSetEnvelope` at `src/data.rs:404-412`, served by `data-service/src/server.rs:106,131`.

**Who does NOT read it:** the executor itself has **zero** shard awareness — `grep shard|ExecutorSet executor/src/executor.rs` returns nothing. An executor executes any `Preload` it receives; there is no "am I in the assigned shard?" check. The finalizer and committer likewise never filter inbound traffic by shard.

### 1b. Config surface for shards

- `src/environment.rs:158-159` `pub shard_count: u32` (doc: "Default 1 = no sharding"); from JSON at `:332-333`, default `1` at `:347`, validated `>= 1` at `:407-409`.
- `src/environment.rs:161-163` `pub shard_quorum_percentage: f32` — **exists but never read.** Its own doc comment claims *"Used by the signature collector when accumulating stake."* That is false: the only reads of this field anywhere are in `src/environment.rs` itself (load `:307`, validate `:389-392`) and test fixtures. `SignatureCollector` receives `env_data.quorum_percentage`, not the shard one.
- Only shipped value in the repo: `deploy/config/env/env.json:18` `"shard_count": 1`. Every other `shard_count: 1` is a test fixture. `shard_count: 2` appears **only** in `sentinel/src/sentinel/tests/helpers.rs:113` and `processing.rs:817,872`.

### 1c. Dead code written for sharding

`src/errors.rs:295-332` — `ExecutorRoutingError { EmptyExecutorSet, ShardCountZero, ShardOutOfBounds(u32,u32), NoExecutorsInShard(u32) }`, with a `From` into `PneumaticError`. **Never constructed in production code** — the only hits are its own `Display` impl and its own tests (`errors.rs:557-573`). The sentinel uses a generic `SentinelError::Routing(String)` instead (`processing.rs:169,172,188,191`).

### 1d. `committee` — does not exist

`grep -rn "committee" --include="*.rs"` over all eight crates: **zero matches.**

### 1e. `partition` — real, but a *storage* namespace, not a shard

- `src/environment.rs:132` `token_partition_id: String`, resolved in `load_from_spec` from the partition list (`environment.rs:186-213`, `EnvironmentPartitionType::{Token,Contract,ProxyAuth,Slush,Other}`).
- `src/config.rs:69` `reconciliation_partition_id: String` — **set, stored, and never used for anything.** Only reads are `config.rs:171` (assignment) and its own tests.
- Partitions are the data-service key prefix: `data-service/src/store.rs:100-110` `get/put(partition_id, key)`. `ActionRouter` uses `env.token_partition_id` purely as the DataProvider key (`src/action_router.rs:129,151,180`).
- `src/tokens.rs:336` `MintArgs.partition_id` — again the storage partition for a mint.

These are **one-per-environment**, not per-shard, and there is no notion of a partition owning a node set.

---

## 2. How a message picks its recipients today

### 2a. The primitive is pure role-bucket fan-out

`NodeRegistry` holds five `DashMap`s, one per role (`src/node/registry.rs:26-31`), and `get_nodes` maps a `NodeRegistryType` to its bucket (`registry.rs:285-293`).

`send_to_all` (`src/node/registry/fanout.rs:13-80`) does exactly:

```rust
let Some(nodes) = self.get_nodes(node_type) else { return };
let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();
```

then sends to **every entry in the bucket**. There is no filter of any kind — no shard, no token, no hash range, no stake, no role sub-selection. Same for `send_to_all_blocking` (`fanout.rs:87-164`, sequential). The only added logic is a per-send timeout (`fanout.rs:36,67`) and failure counters (`record_delivery_failure`, `registry.rs:127-141`).

### 2b. `Gossiper` is inbound-only in production

`Gossiper::send_to_type` (`src/gossiper.rs:154-176`) is the only send path and it iterates the whole bucket:

```rust
for entry in nodes.iter() {
    let sender = sender_for(entry.value());
    sender.get_response(data) ...
}
```

Its **only callers are `tests/transport_integration.rs:269,311,336,368,397,434`** — zero production callers. Two `Gossiper` fields are also write-only: `node_type` (`gossiper.rs:18`, never read) and `conn_factory` (`gossiper.rs:22`, constructed at `:50`, never used). Dedup is on `hash(sender_key, body)` (`gossiper.rs:116,140-147`) — content-based, with no shard/chain component.

### 2c. Every role-crate fan-out site, and its bucket

| Crate / file:line | Action | Bucket targeted |
|---|---|---|
| `sentinel/src/transaction_notifier.rs:42` | `Preload` | **Executor — all** (`send_to_nodes`) |
| `sentinel/src/transaction_notifier.rs:66-82` | `Preload` | **Executor — per-key** (the only shard-aware send) |
| `sentinel/src/transaction_notifier.rs:105` | `Preload` | Finalizer — all (**`finalizer_key` arg ignored**) |
| `sentinel/src/transaction_notifier.rs:137` | `SignShielded` | Finalizer — all (**`finalizer_key` arg ignored**) |
| `sentinel/src/transaction_notifier.rs:152` | `Clear` | Sentinel — all |
| `sentinel/src/transaction_notifier.rs:167` | `Delete` | Sentinel — all |
| `sentinel/src/transaction_notifier.rs:186` | `FinalizerRequest` | Finalizer — all |
| `sentinel/src/transaction_notifier.rs:216-220` | `FinalizerRequest` | **one Finalizer, by key** |
| `executor/src/executor.rs:867` | `Preload` | Finalizer — all |
| `executor/src/executor.rs:904` | `Sign` | Finalizer — all |
| `finalizer/src/message_dispatcher.rs:71` | `Commit` | Committer — all |
| `finalizer/…/message_dispatcher.rs:105` | `ShieldedVote` | Finalizer — all |
| `finalizer/…/message_dispatcher.rs:133` | `Clear` | Sentinel — all |
| `finalizer/…/message_dispatcher.rs:166,170,175` | `BlockFinalized` | Committer, Archiver, Sentinel |
| `finalizer/…/message_dispatcher.rs:203-209` | `BlockConfirmed` | Committer, Archiver, Executor, Sentinel |
| `finalizer/…/message_dispatcher.rs:239-245` | `BlockQuorumReached` | Committer, Archiver, Executor, Sentinel |
| `committer/src/committer/quoruming.rs:136-139` | `BlockQuorumReached` | Committer, Archiver, Executor, Sentinel |
| `committer/src/committer/quoruming.rs:164-167` | `BlockConfirmed` | Committer, Archiver, Executor, Sentinel |
| `committer/src/block_services.rs:135` | `DistributeBlock` | Archiver — all |
| `committer/src/block_services.rs:167` | `DistributeToken` | Committer — all |

This table is independently corroborated in-repo by `testnet-gen/src/topology.rs:83-96`, whose `FANOUT` constant was transcribed from these same call sites and states: *"the pipeline's send graph is a full mesh **minus executor↔executor**."*

### 2d. Is there any destination-specific notion? Yes — but only *per-public-key*, not per-shard

Two mechanisms bypass `send_to_all`:

1. `send_to_shard_executors_for_preload` (`transaction_notifier.rs:46-84`) — loops the shard's keys and does `reg.get_nodes(&Executor).get(&key)` then a raw `conn.send`. **So per-node targeting is achievable today by key lookup into the role bucket — that is the existing lever.** It does not need a new registry dimension, but note the delivery is fire-and-forget: one detached OS thread + a fresh `current_thread` Tokio runtime *per key* (`:70-81`), errors discarded, and `Ok(())` returned even if the key was absent.
2. `request_single_finalizer` (`transaction_notifier.rs:191-223`) — same shape for one finalizer.

**Bug/stub worth flagging:** `send_to_finalizer_for_preload` (`:87-106`) and `send_sign_shielded` (`:119-138`) both take a `finalizer_key: &[u8]` parameter and **discard it**, calling `send_to_nodes(NodeRegistryType::Finalizer, …)` instead. The sentinel computes an assigned finalizer (`assign_finalizer_deterministic`, `sentinel/src/sentinel/finalizing.rs:150-183`) and delivers to it only for `FinalizerRequest` (`finalizing.rs:128`); the `Preload`/`SignShielded` legs broadcast to all finalizers regardless.

**Verdict:** routing is role-bucket fan-out, with two hand-rolled per-public-key exceptions. Nothing is filtered by shard, token, or hash range.

---

## 3. Whether the wire message carries routing coordinates

`src/messages.rs:10-26` — the complete struct:

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Message {
    /// Target environment / token blockchain identifier
    pub chain_id: String,
    /// Action to perform (e.g., "Process", "Confirm", "Reject", "Register")
    pub action: String,
    /// MsgPack-serialized action body
    pub body: Vec<u8>,
    /// Signature over the message body
    pub signature: Vec<u8>,
    /// Public key of the sender
    pub public_key: Vec<u8>,
    /// Stake set for quorum gossip — populated only on "BlockFinalized" messages.
    #[serde(default)]
    pub stake_set: Option<StakeSet>,
}
```

And `src/messages.rs:30-36`:

```rust
pub struct MessageBody<T> {
    pub action: String,
    pub body: T,
}
```

`MessageBody<T>` is **declared and never used anywhere** in the workspace (`grep MessageBody` → only the definition).

Answering directly:

- **Token id:** not on the envelope. `chain_id` is *documented* as "environment/token blockchain" but is never a token id: the sentinel passes `env.token_partition_id` (a constant string, `"token"`, for *every* transaction — `transaction_notifier.rs:34,54,97,129,…`) while the executor/finalizer/committer pass `env_id`/`environment_id` (`executor.rs:858`, `message_dispatcher.rs:60`, `quoruming.rs:125`). So the two halves of the pipeline disagree on what the field means, and it never distinguishes tokens.
- **Shard id:** **absent.** No field, no reserved field.
- **Target role:** **absent.** Delivery role is a property of the *send call site* (`&NodeRegistryType` argument), never of the message.
- **Sequence / nonce:** **absent from the envelope.** `Message::signed` (`messages.rs:47-62`) takes no nonce and there is no replay counter; the gossiper's dedup cache is a TTL cache on `hash(pk, body)` (`gossiper.rs:112-123`, TTL set at construction, e.g. `60` in `node-server/src/node_server/plugins.rs:42`). The transaction nonce lives inside the body: `Transaction.sequence_number` (`src/transactions.rs:172`).
- **Is `chain_id` ever read?** No — outside its own tests, the only hits are the struct definition (`messages.rs:13`), the constructor (`:57`), and a test-fixture construction (`role_dispatcher.rs:258`). **Inbound routing keys exclusively on `action`**: `committer/src/committer.rs:272` `match message.action.as_str()`, and `node-server/src/role_dispatcher.rs:138-156`, whose own doc says *"it inspects only the `action` string."*

**Conclusion:** the wire carries no destination coordinate beyond an action string plus opaque body bytes.

---

## 4. How tokens relate to nodes

**One blockchain per token — real and load-bearing.** `src/tokens.rs:14-42`:

```rust
/// A token IS its own blockchain — independent parallel ledgers.
pub struct Token {
    pub id: Vec<u8>,
    pub metadata: HashMap<String, String>,
    /// This token's independent blockchain
    pub blockchain: Blockchain,
    …
    pub security_level: usize,
    pub is_self_verified: bool,
    pub block_validation_spec_name: String,
    /// Environment ID this token belongs to.
    pub environment_id: String,
    pub sequence_number: usize,
}
```

- Token → environment mapping is `environment_id: String`, one hop, **no shard field on `Token` at all.** Token → storage partition is via `DataProvider::get_token(&tx.token_id, &env.token_partition_id)` — e.g. `sentinel/src/sentinel/processing.rs:89`, `sentinel/src/transaction_validator.rs:48`, `sentinel/src/sentinel/shielded.rs:70`.
- **Per-token state does exist, but as an in-process cache, and only in the committer:** `committer/src/committer.rs:112` `tokens: Arc<DashMap<Vec<u8>, Token>>` — keyed by token id, no shard component. Sentinels/ executors / finalizers hold no token map; they fetch per-token through the data service.
- Global (non-token) state is deliberately split out: `src/user.rs:5-16` `User { public_key, fuel_balance /* "global across all tokens" */, stake /* "Global stake" */, nonce /* "per-sender, not per-token" */ }`. Per-token balances live in `Token.asset_data` as `Account` (`user.rs:39-45`).
- Storage shape: `data-service/src/store.rs:100-110` is a flat `(partition_id, key) -> value` map. Partition dimension = `token` / `reconciliation` / …; key = entity id (token id, or big-endian epoch for snapshots — `data-service/src/server.rs:95`).
- Note a latent wart: `DefaultDataProvider::latest_block_hash(partition_id)` (`src/data.rs:301-307`) does `let token_id = partition_id.as_bytes().to_vec()` — it treats the *partition string* as a token id. The shard path feeds it `environment_id` (`processing.rs:177`), so the "mined tip" salt for shard selection resolves against a token literally named `"env"`; on miss it collapses to an empty salt via the double `.unwrap_or_default()` at `processing.rs:178-179`.

**Answer:** yes, one chain per token; token→environment is one string hop; token→shard does not exist; per-token state is real but committer-cached, and the shard salt is sourced through a type-confused lookup.

---

## 5. Quorum and stake scope — every quorum is GLOBAL

### `to_stake_set` — the whole stake store, filtered only for zeros

`committer/src/epoch_manager.rs:78-93`:

```rust
pub fn to_stake_set(&self) -> StakeSet {
    StakeSet {
        stakers: self.stakes.iter()
            .filter(|kv| *kv.value() > 0)
            .map(|kv| (kv.key().clone(), *kv.value()))
            .collect(),
    }
}
```

`StakeSet::total_stake` (`src/epoch/stake_sets.rs:19-23`) = `stakers.values().fold(0, saturating_add)` — sum over **all** stakers. `ExecutorSet::total_stake` (`stake_sets.rs:78-82`) likewise.

### Block-confirmation quorum — divided by the global total

`committer/src/committer/quoruming.rs:55-74`:

```rust
let total_stake = { let cache = self.stake_set_cache.lock().await;
                    cache.get(&block_hash).map(|ss| ss.total_stake()) };
if let Some(total) = total_stake {
    let quorum_pct = self.env_data.quorum_percentage.round() as u128;
    let reached = (cumulative_stake as u128) * 100 >= (total as u128) * quorum_pct;
```

`total` is the `StakeSet` that rode in on `BlockFinalized` — i.e. the environment-wide set. `shard_quorum_percentage` is not consulted.

### Finalizer count quorum — global voter count

`finalizer/src/signature_collector.rs:76-93`:

```rust
let sig_count = self.signature_registry.get_transaction_registry(tx_id).map(|m| m.len()).unwrap_or(0);
if self.total_voters == 0 { return Ok(false); }
let quorum_pct = self.quorum_percentage.round() as u128;
let reached = (sig_count as u128) * 100 >= (self.total_voters as u128) * quorum_pct;
```

`total_voters` originates at `finalizer/src/finalizer/finalizing.rs:48-52`:

```rust
pub(crate) fn resolve_stake_metrics(&self) -> (u64, u32) {
    match self.get_stake_set_for_epoch() {
        Some(set) => (set.total_stake(), set.stakers.len() as u32),
        None => (0, 0) } }
```

— the whole epoch stake set. **And in the composite node the numbers are hard-coded**: `node-server/src/node_server/plugins.rs:119-134` passes literal `66.6` and `4` for `quorum_percentage`/`total_voters`, ignoring `env_data.quorum_percentage` entirely.

### Finalizer stake quorum — global total again

`finalizer/src/signature_collector.rs:107-129`: `admitted` = sum of `current_stake` over collected votes; `Ok(admitted * 100 >= (total_stake as u128) * quorum_pct)`, with `total_stake` supplied by the caller from the same global snapshot.

### Leader selection — global, one leader per epoch

- Core `LeaderSelector` (`src/epoch/leader.rs:252-271`) → `deterministic_select(stakers, LEADER_DOMAIN, &[], epoch, prev_hash)`; `leader.rs:115-124`:

```rust
let total = stakers.total_stake();
if total == 0 { return None; }
let seed = derive_selection_seed(domain, epoch_number, prev_block_hash, seed_bytes);
let mut rng = StdRng::from_seed(seed);
let target: u64 = rng.gen_range(0..total);
```

then a sorted cumulative-stake walk (`leader.rs:132-150`).
- Production selector is a second implementation: `committer/src/epoch_manager.rs:296-367`, `select_internal` with the same arithmetic (`:314` `let total = stakers.total_stake();`, `:339` `rng.gen_range(0..total)`).
- Its input is global: `committer/src/committer/epoching.rs:80` `let stake_set = self.stake_store.to_stake_set();` → `advance_epoch_to` (`epoching.rs:123-128`).
- `Epoch` carries exactly one leader — `src/epoch/types.rs:15-24` `leader_public_key: Vec<u8>`. **No per-shard leader field exists.**
- Finalizer assignment is likewise global: `assign_finalizer_deterministic` selects from `EpochSnapshotCache<StakeSet>` with `FINALIZER_DOMAIN` and `tx_id` as salt (`sentinel/src/sentinel/finalizing.rs:158-172`) — over the **whole** stake set, not a shard.

**Answer:** leader selection, finalizer assignment, executor-shard assignment, transaction quorum, and block-confirmation quorum are **all computed against the global stake set**. The only thing that ever narrows is executor *selection for the preload hop*.

---

## 6. The registry's data shape

`src/node/registry.rs:26-89` (fields; abridged comments):

```rust
pub struct NodeRegistry {
    committers: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    sentinels:  Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    executors:  Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    finalizers: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    archivers:  Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    config: Arc<Config>,
    network: Option<Arc<RnsNetwork>>,
    stake_check: StakeCheck,
    evictor: Mutex<Option<JoinHandle<()>>>,
    shutdown: Arc<AtomicBool>,
    evict_interval: Duration,
    delivery_failures: Arc<DashMap<([u8; 16], NodeRegistryType), u64>>,
    query_nonces: Arc<DashMap<([u8; 16], [u8; 16]), Instant>>,
    send_timeout: Duration,
    admission_lock: Arc<std::sync::Mutex<()>>,
    peering: Mutex<Option<JoinHandle<()>>>,
    declared_roles: std::sync::RwLock<Vec<NodeRegistryType>>,
}
```

`Nodes = Arc<DashMap<Vec<u8>, NodeRegistryNode>>` (`src/node.rs:89`), keyed by the peer's Ed25519 public key. `NodeRegistryNode` (`src/node.rs:91-107`) carries `rhash`, `conn`, `last_seen`, and three directory-binding fields — no shard.

`NodeRegistryType` (`src/node.rs:147-153`) has exactly five variants: `Committer, Sentinel, Executor, Finalizer, Archiver`. `is_consensus_role` (`:166-168`) is just "not Archiver".

**Is there ANY dimension besides role?** **No filing dimension besides role.** The filing key is `(role-bucket, public_key)` — nothing else. It therefore **cannot** hold "executors of shard 3" as a separate bucket from "executors of shard 7". *But* — and this matters for the design — because the bucket is a `DashMap` keyed by public key, a caller that already knows the shard's key list can address members individually without touching the registry shape; that is precisely what `send_to_shard_executors_for_preload` does (`transaction_notifier.rs:76-79`). A shard *index* dimension would be new; per-*member* delivery is not.

`declared_roles` (`registry.rs:74-88`, seeded `:195`) is also purely role — it advertises which role buckets this node serves; it carries no shard.

**`NodeTypeConfig { min, max, min_stake }` — `src/node.rs:21-25`:**

- Semantics are documented at `src/config.rs:390-393`: "the minimum/maximum number of registered nodes for that type and the minimum stake required to join that registry."
- **`max` bounds admission** through `Config::get_max_node_number` (`src/config.rs:342-346`, returns `node.max`, **or `0` when the type has no entry** — i.e. a missing entry means admit nothing). Five enforcement points, all `nodes.len() >= get_max_node_number(type)`:
  - `src/node/registry/registration.rs:57` `type_is_maxed_out`
  - `:84` `register_peer` (returns `false`)
  - `:125` `register_peer_with_binding`
  - `:609` `admit_node_under_type` — the control-plane path, with the capacity check and the insert held under `admission_lock` (`:603-609`, Phase 6.3 over-admission fix)
  - `:912` the directory-seeded insertion path
- **`min_stake`** is the per-type registration gate, via `Config::get_min_type_stake` (`config.rs:348-353`) and the injected `StakeCheck` (`registry.rs:24`, invoked at `registration.rs:596`), backed by `StakeIndex` (`src/node/stake_index.rs:53-67`) — which indexes the **global** current-epoch `StakeSet` under `token_partition_id` (`stake_index.rs:59-61`). Second floor: `CostModel.global_min_stake` via `get_global_min_stake` (`config.rs:362-366`) and `meets_minimum_stake` (`config.rs:459-462`).
- **`min` is never read.** `grep` for any read of the `.min` field of a `NodeTypeConfig` across all crates returns nothing. It is set at `config.rs:400-404` and in fixtures (`sentinel/src/transaction_notifier.rs:357,361`) and then ignored. **Dead field.**
- Default values are **hard-coded, not JSON**: `config.rs:394-408` sets `{ min: 1, max: 1000, min_stake: 10 }` for every type; `default_min_stake() -> 10` (`:409-411`). The only JSON-overridable part is the *minimum stake*, via `CostModel.per_type_min_stake` (`config.rs:110-121`).

---

## 7. Existing config surface for shard counts

**A config author can set exactly two shard-related numbers today**, both in the per-environment spec:

| Key | Where parsed | Default | Read by |
|---|---|---|---|
| `shard_count` | `src/environment.rs:332-333` | `1` (`:347`) | `sentinel/src/sentinel/processing.rs:152,183` — **only** |
| `shard_quorum_percentage` | `src/environment.rs:335-336` | `67.0` (`:348`) | **nobody** (validated at `:389-392`, loaded at `:307`) |

Validation: `shard_count >= 1` (`environment.rs:407-409`), `shard_quorum_percentage ∈ (0,100]` (`:389-392`). Fail-closed at boot; documented at `environment.rs:357-370`. Shipped example: `deploy/config/env/env.json:18-19` (`shard_count: 1`, `shard_quorum_percentage: 67.0`).

**`config.json` has no shard surface at all.** `ConfigSpec` (`src/config.rs:463-513`) fields are: `public_key` (ignored, `:465-472`), `is_full_node`, `rest_api_version`, `environments`, `main_env_id`, `reconciliation_partition_id`, `identity_path`, `bootstrap_peers`, `rns_port`, `ip_address`, `mesh_fragment_path`, `directory_observer`, `transport_enabled`. Both shipped files (`deploy/config/committer/config.json`, `deploy/config/full-node/config.json`) contain only these.

`testnet-gen` sets per-role counts and mesh topology, **not** shard counts. `GenSpec` (`testnet-gen/src/emit.rs:22-51`) → `counts: [(Role, usize); 4]`, `mode: TopologyMode`, `role_graph_above`, `stake`, `fuel_balance`, `env_template`, `placement`, … Flags (`testnet-gen/src/main.rs:36-60`): `--validators`, `--sentinels`, `--executors`, `--finalizers`, `--committers`, `--topology full-mesh|role-graph|auto`, `--role-graph-above`, `--base-port`, `--stake`, `--fuel`, `--addresses[-file]`, `--bind-ip`, `--env-template`, `--data-addr`, `--force-keys`. Crucially, the generator copies the env template **verbatim** except `log_file` and `environment_id` (`testnet-gen/src/emit.rs:286-301`: *"Everything else — quorum, risk, shard count, … — is copied verbatim"*), so `shard_count` on a generated node equals whatever is in the single template — **the same value for every node**. There is no way to emit a per-node shard assignment, because the wire, the registry, and `Token` have no place to put one.

`Role` is a 4-variant generator enum (`testnet-gen/src/topology.rs:36-41`: Sentinel/Executor/Finalizer/Committer) mapping to `NodeRegistryType` by string (`registry_type_str`, `:46-54`). No shard analogue.

---

## No representation at all today

Concepts a "sharded clusters with dense interiors" design must introduce from scratch:

1. **Shard identity on the wire.** `Message` (`src/messages.rs:11-26`) has no shard id, no target-role field, no sequence/nonce, and its `chain_id` is never read and never carries a token id. Every receiver routes on `action` alone.
2. **Shard membership as a node/registry attribute.** `NodeRegistry` files a peer by `(role bucket, public key)` only (`registry.rs:26-31`, `node.rs:89,91-107`). No shard dimension on `NodeRegistryNode`, `Registration`, `NodeRegistryEntry`, `NodeRequest`, or `declared_roles`. A peer cannot advertise, or be filed under, a shard.
3. **Shard-scoped quorum.** `shard_quorum_percentage` is parsed and validated but never read; every division uses a global `total_stake()` / `total_voters` (`quoruming.rs:57,73-74`; `signature_collector.rs:83,90-91,126-128`). No shard denominator exists anywhere.
4. **Shard-scoped leader / committee election.** `Epoch.leader_public_key` is a single global key (`src/epoch/types.rs:15-24`); `IEpochLeaderSelector::select` (`types.rs:98-106`) takes the global `StakeSet` and returns one key. No per-shard leader, no committee struct. The word `committee` does not appear in the codebase.
5. **Shard-scoped finalizer / committer assignment.** Sharding covers executor preload only. Finalizer assignment draws from the **global** stake set (`sentinel/src/sentinel/finalizing.rs:158-172`), the executor broadcasts its vote to **all** finalizers (`executor.rs:867,904`), and commits/confirmations broadcast to all roles (§2c).
6. **Receiver-side shard validation.** No executor, finalizer, or committer checks that it belongs to the addressed shard before acting (`grep` over `executor.rs`, `finalizer.rs`, `committer.rs` finds no shard reference). Sharded routing is a send-side optimisation with no enforcement, so a mis-routed message is processed normally.
7. **Token→shard mapping.** `Token` (`src/tokens.rs:17-42`) has `environment_id` but no shard field; `token_partition_id` is a one-per-environment storage prefix (`environment.rs:132`, `data-service/src/store.rs:100`). There is no way to say "shard 3 owns these tokens."
8. **Materialised shard structure.** Shards are recomputed per transaction inside `deterministic_select_shard` and thrown away (`leader.rs:222-241`). There is no `Shard` type, no epoch-persisted shard map, no way to ask "who is in shard 3" without replaying the shuffle for every transaction. `ExecutorSet` stores only a flat `HashMap<Vec<u8>, u64>` (`stake_sets.rs:72-75`).
9. **Shard-aware topology / density.** `testnet-gen` links by role (`topology.rs:83-96` `FANOUT`) with an explicit note that `RoleGraph` *"is not a density fix"* and removes only the executor↔executor class out of ten. No config emits per-shard peer sets; the env template (and hence `shard_count`) is copied verbatim to every node (`emit.rs:286-301`).
10. **Shard-scoped storage/state.** The data service is a flat `(partition_id, key)` map (`store.rs:100-110`); no shard-scoped partition, chain, or snapshot key exists.
11. **Per-shard observability.** Telemetry/mesh fragments key failures by `(rhash, NodeRegistryType)` (`registry.rs:45`, `fanout.rs` `record_delivery_failure`). No shard-tagged metric exists, and the shard-aware send path bypasses this accounting entirely (`transaction_notifier.rs:66-83` discards every result).

### Two adjacent defects found while mapping (not sharding, but in the routing path)

- `send_to_finalizer_for_preload` (`transaction_notifier.rs:87-106`) and `send_sign_shielded` (`:119-138`) accept a `finalizer_key` argument and **ignore it**, broadcasting to the whole Finalizer bucket — so the sentinel's deterministic finalizer assignment is silently undone on those two legs.
- `MessageBody<T>` (`messages.rs:31`) is dead; `ExecutorRoutingError` (`errors.rs:301`) is constructed nowhere; `Gossiper::{node_type, conn_factory}` (`gossiper.rs:18,22`) are written and never read; `Gossiper::send_to_type` (`:154`) has no production caller; `reconciliation_partition_id` (`config.rs:69`) and `NodeTypeConfig.min` (`node.rs:22`) are set and never read.
