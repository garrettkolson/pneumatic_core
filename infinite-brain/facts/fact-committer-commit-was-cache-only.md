---
id: fact-committer-commit-was-cache-only
title: "The committer's commit was a cache event, not a state change: token cache empty at boot and post-append writes never persisted — until Phase 1 fixed both, no committed block was ever observable through the data service, and a fresh committer could not commit at all"
type: fact
namespace: pneumatic
visibility: namespace
summary: "FOUND AND FIXED 10/07/2026 (roadmap Phase 1, prerequisite of the ingress exit test). `BlockServices::commit_block` read and appended ONLY the committer's in-memory token cache: it never called `save_token`, and nothing ever populated that cache in production (`distribute_token` — the only writer — has no production caller; deploy-commit caches the token it creates). Two live-path consequences, both verified in the tree before any code change: (1) a committed block was NEVER observable through the data service — the `DefaultDataProvider` read path returned each token's genesis tip forever, which also pinned ADR-019's selection salt at genesis for every plain-transfer token; (2) a committer that booted fresh (restart, or any node that did not itself deploy the token) answered every `Commit` with `TokenNotFound` — genesis-seeded tokens exist in the data service, not in the empty cache. Tests missed it because the committer test fixtures seeded the cache directly AND wired `BlockServices` to a second, network-touching `DefaultDataProvider` the commit path never read — production parity was never enforced. Fix: cache-warm-on-miss from the provider (fail-closed `TokenNotFound` now names the store), persist the advanced token INSIDE the `get_mut` guard (a save outside would let a concurrent commit's later append land before the earlier save — stale-overwrite reorder), surface persistence failure as new `CommitterError::TokenPersist` (commit reported success despite nothing durable is a lie). Mutation-verified: removing the save fails the persistence unit test, the cold-warm test, and the client→ingress→commit E2E at `left: 1`; disabling the warm fails both cold-cache tests and the E2E."
auto_inject: true
applicable_when: "Any change to committer commit/persist paths, the token cache, data-service reads of chain state, restart-recovery reasoning, or anything that reads a token's chain from the provider expecting committed history"
confidence: 0.95
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If `commit_block` stops calling `save_token` inside the `get_mut` guard or starts swallowing its error; if the cache-warm path stops reading `data_provider.get_token` on miss; if a new committer code path mutates tokens in the cache without persisting; if test fixtures again wire `BlockServices` a provider different from the `Committer`'s"
tags: [fact, committer, persistence, data-service, token-cache, defect, resolved, phase-1, silent-failure]
edges:
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.9
    note: "Phase 1's exit test ('committed block observable through the data service') was unsatisfiable through this gap — finding it is what made the ingress exit test real"
  - target: decision-transaction-ingress-http-edge
    type: related_to
    weight: 0.85
    note: "The ingress E2E is what proved the fix: a fresh provider over a real socket sees genesis+committed only if the commit persisted; with the save removed it sees 1 block"
  - target: decision-selection-salt-per-token-tip
    type: related_to
    weight: 0.8
    note: "ADR-019 salts selection at the token's chain tip — with no persistence that tip was the genesis block forever for every transfer token, quietly degrading salt freshness"
  - target: fact-data-service
    type: depends_on
    weight: 0.75
    note: "The data service is where chain state is supposed to live; this is the concrete cost of the committer treating it as write-only-for-deploys"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.7
    note: "Two of the three faces were failures reporting success: Ok(()) after an unpersisted append; TokenNotFound blamed the cache while the store held the token"
related: []
source_url: "repo:committer/src/block_services.rs, repo:committer/src/committer/tests/commit.rs, repo:node-server/src/node_server/tests/e2e.rs"
---

# The committer's commit was a cache event, not a state change

## What the code did (verified 10/07/2026, before any change)

`BlockServices::commit_block` (the terminal step of every transaction's `Commit`):

1. `self.tokens.get_mut(&token_id)` — the committer's in-process DashMap cache — and on a miss returned `TokenNotFound`. Nothing in production ever populated this cache: `BlockServices::distribute_token` (the only cache writer) has **no production caller** (grep of both binaries and all four role crates); the deploy-commit path caches only the token it itself created. So a fresh committer — any restart, any node that did not personally deploy the token — answered every `Commit` for a genesis-seeded token with `TokenNotFound`. The error blamed the cache while the data service held the token.
2. On a cache hit: append the block to the in-memory `Blockchain` and return `Ok`. **No `save_token` anywhere.** `save_token` on the committer side existed only in the deploy/storage-delta paths — plain transfers, the entire ordinary transaction flow, wrote nothing back.

Consequences on the live path: a committed block was never readable through `DefaultDataProvider::get_token` (the read path every role and every operator uses); ADR-019's per-token-tip selection salt was permanently pinned at each token's genesis tip for transfer tokens; and a committer restart silently lost all committed chains. The E2E-shaped truth: the Phase-1 exit test ("committed block observable through the data service") could not have passed before this fix, for any client surface.

## Why the suite never saw it

The committer test fixtures seeded the token cache directly (mirroring what production never does) and — the subtler half — constructed a **second** provider for `BlockServices` (`DefaultDataProvider::new()`, which dials the ambient local data-service socket) while handing the `Committer` the injected in-memory test provider. Production injects ONE provider into both (`build.rs:177`, `main.rs:321`); the fixture's split meant even the first real provider-touch from `commit_block` hit real I/O and failed. Fixing the fixture to production parity (same `Arc` into both) was itself part of the change — the playbook's rule that the fixture must match the live wiring, in code.

## The fix (one commit with the ingress, 10/07/2026)

- **Cache-warm on miss:** `commit_block` reads `data_provider.get_token(token_id, token_partition)` — the same key every other role reads — and fails closed with a `TokenNotFound` that *names the store* in its message.
- **Persist inside the guard:** the advanced token is `save_token`-ed while still holding the `get_mut` guard. Outside the guard, commit A's save could land after commit B's append-then-save, writing B's predecessor over B — a stale-overwrite reorder; inside it, the append and its persist are atomic with respect to the cache entry.
- **`CommitterError::TokenPersist`:** a persistence failure now aborts the commit with the token id and the store error. `Ok` while nothing durable happened is exactly the class of lie this vault keeps finding on this path.

Residuals named honestly: the whole `Token` is re-serialized per commit (size cost grows with chain length — a delta-persistence design is future work); this fix persists chain + token state, and the gas/user path already persisted through the provider.

## How each claim was checked

- Live-path claims: read `commit_block`, grepped all callers of `distribute_token`/`save_token` across both node binaries and all role crates (presence-grep of the writers, not absence-grep of a symptom); compared fixture wiring against `build.rs`/`main.rs` wiring line by line.
- Fix claims, mutation-verified (each mutant restored after): removing the `save_token` → `committed_block_persists_the_advanced_token_to_the_data_service`, `cold_cache_commit_warms_the_token_from_the_data_service`, and the client-ingress E2E all fail (E2E asserts `left: 1` — genesis only); disabling the warm → both cold-cache tests and the E2E fail; swallowing the persist error → `commit_surfaces_token_persist_failure_instead_of_reporting_success` fails.
- End-to-end: `client_transaction_through_the_ingress_commits_observable_through_the_data_service` boots a real `pneumatic_data_service` behind `DefaultDataProvider`, leaves the committer cache EMPTY, and reads genesis+committed through a *fresh* provider — the persistence is proven through the production read path, and the result hash is recomputed outside the pipeline (ADR-019 exit-test discipline). Suite total moved 1156/37/0 → 1179/37/0.
