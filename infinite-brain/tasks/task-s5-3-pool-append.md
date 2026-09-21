---
id: task-s5-3-pool-append
title: "S5.3 — Committer: pool append, persistence, nullifier commit"
type: task
namespace: pneumatic
visibility: namespace
summary: "Wire the committer to apply shielded pool updates at commit: authoritative proof re-check, idempotent replay, first-spent-wins StaleNullifier, K-freshness, atomic rollback, durable nullifiers."
auto_inject: false
applicable_when: "Implementing or reviewing the next shielded phase (S5.3) in the committer"
confidence: 0.95
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Close when the S5.3 entry lands in AUDIT_CHECKLIST.md with its Done note"
tags: [task, shielded, committer, pool, nullifier, s5]
edges:
  - target: source-shielded-plan
    type: derived_from
    weight: 1.0
    note: "Specified in the S5.3 section (lines 807-894)"
  - target: concept-nullifier
    type: depends_on
    weight: 0.9
    note: "Durable nullifier commit is the heart of the item"
  - target: concept-incremental-merkle-tree
    type: depends_on
    weight: 0.8
    note: "Appends commitment leaves under the single-writer guard"
  - target: concept-merkle-root-freshness
    type: depends_on
    weight: 0.7
    note: "Commit step 3 enforces the K-recency window"
  - target: pattern-fail-closed
    type: depends_on
    weight: 0.8
    note: "Boot with missing/corrupt pool state is an error, never start empty"
  - target: event-s5-2-finalizer-wiring
    type: related_to
    weight: 0.7
    note: "S5.2's votes bind the referenced (pre) root; the post root is computed here"
  - target: pillar-shielded-value-transfer
    type: part_of
    weight: 0.9
    note: "The next landing item of the Tier-1 shielded plan"
related: []
source_url: "repo:pneumatic-shielded-implementation-plan.md"
---

# Task: S5.3 committer pool append + nullifier commit

Specified in the shielded implementation plan, **S5.3 — Committer: pool state persistence + nullifier commit** (lines 807–894).

The pool is ONE global committer structure behind a **single-writer guard** (the Phase 5.4 single-epoch-writer pattern). It holds the Merkle tree (S1.6), the nullifier set (S4.2), and an **applied-update map** `(block_hash → {leaves appended, nullifiers marked})` that makes replay idempotent and rollback exact.

At commit, the guarded `apply_pool_update(block)` runs in order: (0) re-run `verify_shielded_proof` — the committer's own check is the final, authoritative consensus gate; (1) idempotent no-op if the block's hash is already in the map; (2) `StaleNullifier` reject if any nullifier was marked by a *different* block; (3) `referenced_root` within the K-recency window (K is a consensus parameter); (4) `mark_many_atomic` + append commitments + compute the post root + record the delta, atomically with the chain append.

Hook sites: the commit path (`committer.rs:432/461/527-535`, `block_services.rs:67-105`) and `handle_block_finalized` (`committer.rs:672`), plus an H12 payload hash-match extension (`committer.rs:519`). Rollback unwinds pool state in lockstep with `blockchain.remove_block()`. Durability ordering: nullifiers are marked durable **before** the block is reported committed.
