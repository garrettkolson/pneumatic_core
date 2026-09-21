---
id: source-shielded-roadmap
title: "Source: plans/pneumatic-shielded-roadmap.md"
type: source
namespace: pneumatic
visibility: namespace
summary: "The shielded roadmap: Tier-1 shielded value transfer (deliverable) vs Tier-2 zk-VM (out of scope), key decisions, S-phase plan, and open viewing-key/compliance question."
auto_inject: false
applicable_when: "Checking shielded phase status, scope, or recorded decisions"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the roadmap file is edited — re-verify phase status against git log"
tags: [roadmap, source, shielded, planning]
edges:
  - target: pillar-shielded-value-transfer
    type: supports
    weight: 1.0
    note: "Defines the Tier-1 deliverable"
  - target: fact-shielded-stack
    type: supports
    weight: 0.9
    note: "Phase numbering for the landed stack"
  - target: question-viewing-keys-compliance
    type: supports
    weight: 0.9
    note: "Source of the open question"
related: []
source_url: "repo:plans/pneumatic-shielded-roadmap.md"
---

# Source: shielded roadmap

`plans/pneumatic-shielded-roadmap.md` is the authoritative plan for the shielded work. It defines **Tier 1** (shielded value transfer — the deliverable) and **Tier 2** (zk-VM — explicitly out of scope), records the key design decisions (client-side proving, halo2/no trusted setup, note-based UTXO à la Orchard, global pool, consensus-critical nullifiers), sequences the S-phases (S1.1 … S6), and flags viewing keys/compliance as an open *product* decision.

All S-phase status claims in this vault are `derived_from` this file, cross-checked against the git log.
