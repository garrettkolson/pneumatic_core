---
id: fact-rmp-wire-named-maps
title: "rmp wire format is named maps, not positional arrays (migrated 10/01 for the deploy fix)"
type: fact
namespace: pneumatic
visibility: namespace
summary: "serialize_to_bytes_rmp uses rmp_serde::to_vec_named (named maps), not to_vec (positional arrays), since 10/01/2026. Positional arrays + skip_serializing_if shift slots on deploy txs (TypeMismatch(Array16) at commit); named maps make it safe. Reads stay backward-compatible (from_slice accepts both), but block-hash canonical bytes changed."
auto_inject: false
applicable_when: "Reasoning about the rmp wire format, block-hash canonical bytes, the deploy path, skip_serializing_if, or why NON_SHIELDED_BASELINE / the wasm caller fixture changed"
confidence: 1.0
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale if encoding.rs::serialize_to_bytes_rmp switches back to rmp_serde::to_vec (positional arrays) — do not revert without re-auditing every skip_serializing_if field"
tags: [encoding, wire-format, rmp, msgpack, deploy, serialization, protocol]
edges:
  - target: concept-executor-role
    type: related_to
    weight: 0.8
    note: "The executor's Sign votes and the block-hash canonical bytes ride this wire format"
  - target: task-executor-contract-execution
    type: related_to
    weight: 0.85
    note: "The deploy e2e (P10) surfaced this fix; it is part of closing the contract-execution plan"
  - target: fact-wire-protocol
    type: refines
    weight: 0.9
    note: "Specifies the concrete rmp container encoding for the MsgPack wire format"
  - target: fact-test-suite
    type: related_to
    weight: 0.7
    note: "The re-pinned NON_SHIELDED_BASELINE + regenerated wasm fixture are the measured collateral of this change"
related: []
source_url: "src/encoding.rs"
---

# rmp wire format is named maps, not positional arrays

`pneumatic_core::encoding::serialize_to_bytes_rmp` uses **`rmp_serde::to_vec_named`** (named maps)
since **10/01/2026**, not `rmp_serde::to_vec` (positional arrays). This is the one-line deploy fix
landed while closing the contract-execution plan (Phase 10 e2e).

**Why the change was forced.** rmp-serde 1.3.0 serializes a struct as a positional array by default.
`Transaction` puts `skip_serializing_if` on three fields (`payload` / `gas_limit` / `result_data`).
On a deploy tx, `gas_limit == 0` (no cap) and `payload` is empty, so both slots are skipped — and a
positional array has no way to mark a skipped slot, so every later field shifts one slot left. The
committer's `from_slice` then reads `result_data` (an array) into the `gas_limit` (u64) slot and fails
with `TypeMismatch(Array16)`. Named maps key every value by field name, so `skip_serializing_if` is
harmless: an absent key deserializes to the field's default.

**Compatibility.** The deserializer (`deserialize_rmp_to` → `rmp_serde::from_slice`) accepts **both**
containers, so old persisted data (arrays) and new data (maps) both read. There is no wire break on
the read path and no data migration.

**Consequence — block hashes moved.** `canonical_signed_trans_bytes` (the block-hash input) and every
rmp frame now serialize to named maps, so the canonical bytes changed. This is a self-consistent
migration (all nodes run the same code, so block hashes agree across the network), but it is **not**
byte-stable against the pre-10/01 form. The measured collateral, all updated 10/01/2026:
- `NON_SHIELDED_BASELINE` in `src/transactions.rs` re-pinned from 68 (fixarray) to 380 bytes (fixmap)
  — the `non_shielded_canonical_bytes_match_baseline` + `non_shielded_msgpack_field_is_absent` tests.
- `wasm_caller.wasm`'s baked-in genesis snapshot-ref hash regenerated via
  `src/contracts/wasm_fixtures/generate_caller.py` (old `0xDB…E8` → new `0xC7…5D`), because
  `BlockFactory::create_hash` now hashes the named-map form; the `wasm_call_wasm_round_trip` test
  re-passes.

**Do not revert to positional arrays.** `skip_serializing_if` is fundamentally incompatible with them
(any future skip-prone field re-breaks the wire). Named maps are the robust container for structs with
conditional fields.
