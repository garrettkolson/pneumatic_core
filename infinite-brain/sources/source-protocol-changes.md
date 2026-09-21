---
id: source-protocol-changes
title: "Source: PROTOCOL_CHANGES.md (Phase-0 decisions)"
type: source
namespace: pneumatic
visibility: namespace
summary: "Records the resolved Phase-0 protocol decisions: conflict definition, optimistic finality replacing quorum, shared StakeSet, discard-only conflict handling with double-sign slashing."
auto_inject: false
applicable_when: "Checking finality/conflict semantics or why the protocol looks the way it does"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when PROTOCOL_CHANGES.md is edited"
tags: [protocol, decisions, source, finality]
edges:
  - target: concept-optimistic-finality
    type: supports
    weight: 1.0
    note: "The optimistic-commit decision"
  - target: concept-candidate-registry-conflict
    type: supports
    weight: 0.9
    note: "The conflict definition"
  - target: concept-executor-sharding
    type: supports
    weight: 0.8
    note: "Shared-StakeSet election/shuffling decision"
related: []
source_url: "repo:PROTOCOL_CHANGES.md"
---

# Source: PROTOCOL_CHANGES.md

`PROTOCOL_CHANGES.md` documents the **Phase-0 protocol decisions**, all four resolved:

1. **Conflict** = two *valid* blocks with the same `(token_id, previous_hash)`.
2. **2/3 quorum → optimistic commit**: first valid executor signature finalizes.
3. **Same StakeSet** for leader election and conflict voting.
4. **Losers discarded**; slashing only on double-sign.

It is the primary evidence for the finality, conflict, and sharding nodes in this vault.
