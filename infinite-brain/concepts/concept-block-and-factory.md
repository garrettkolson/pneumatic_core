---
id: concept-block-and-factory
title: "Block, Blockchain, and canonical block hashing"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Block (blocks.rs:23) is a signed tx + metadata in an append-only SHA-256 hash chain; BlockFactory::create_hash (blocks.rs:109) hashes 6 canonical fields."
auto_inject: false
applicable_when: "Changing block structure, hashing, chain validation, finality status, or block trimming"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the create_hash input layout, Blockchain storage, or validate_next_block checks change"
tags: [blocks, hashing, chain, canonicalization]
edges:
  - target: concept-per-token-chains
    type: part_of
    weight: 0.9
    note: "Blockchain is the storage engine inside each Token"
  - target: concept-optimistic-finality
    type: related_to
    weight: 0.85
    note: "Every Block carries FinalityStatus Optimistic/Confirmed (blocks.rs:13-20, 30)"
  - target: concept-executor-sharding
    type: related_to
    weight: 0.6
    note: "proposer_key + epoch_number on the block bind it to deterministic routing decisions (blocks.rs:39-42)"
  - target: concept-candidate-registry-conflict
    type: related_to
    weight: 0.6
    note: "Block hashes are what candidates and same-height conflicts compare"
related: []
source_url: "Empty"
---

# Block, Blockchain, and canonical block hashing

`Block` (`src/blocks.rs:23`) wraps one `SignedTransaction` plus `token_metadata`, `previous_hash`, `current_hash`, `timestamp`, `finality_status: FinalityStatus` (Optimistic/Confirmed, lines 13-20), `proposer_key`, and `epoch_number`. `proposer_key` has single semantics (AUDIT C2): always derived from `SignedTransaction.proposer_key` so every constructor agrees, feeding the block hash, stake lookup, and slash-target selection (lines 31-39).

`BlockFactory::create_hash` (`src/blocks.rs:109`) computes SHA-256 over a **canonical** fixed-order concatenation: `previous_hash ‖ timestamp ‖ canonical(signed_trans) ‖ canonical(token_metadata) ‖ proposer_key ‖ epoch_number` (lines 97-108, 132-154). HashMap-backed fields go through sorted-key BTreeMap serialization so equal blocks hash identically regardless of insertion order; `create_hash` now returns `Result` — a canonicalization failure is an error, never a panic.

`Blockchain` is append-only: `add_block` (line 207), O(1) `cached_tip` (line 298), and `validate_next_block` (line 337) which requires chain linkage — genesis convention: block 1 has an **empty** `previous_hash` (lines 53-56, 344-349) — plus a self-hash recomputation that fails closed on hash errors. Tip rollback (`remove_block`) and oldest-trim (`remove_oldest`) support conflict resolution and pruning.
