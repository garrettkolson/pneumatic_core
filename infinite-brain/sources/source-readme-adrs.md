---
id: source-readme-adrs
title: "Source: README.md ADRs (ADR-001…ADR-010)"
type: source
namespace: pneumatic
visibility: namespace
summary: "README's Architecture Design Decisions (lines 102-173): ADR-001…010 — trait abstraction, DashMap registries, deterministic election/routing, optimistic finality, conflict resolution, sharding."
auto_inject: false
applicable_when: "Looking up why a non-shielded consensus mechanism was built the way it was, before the shielded plan's decisions"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when README.md's ADR section is edited"
tags: [readme, adr, source, consensus, architecture]
edges:
  - target: concept-optimistic-finality
    type: supports
    weight: 0.9
    note: "ADR-005/ADR-010 (lines 128-172) are the originating decisions"
  - target: concept-candidate-registry-conflict
    type: supports
    weight: 0.9
    note: "ADR-008 (lines 146-160) defines conflict at chain-position level"
  - target: concept-executor-sharding
    type: supports
    weight: 0.9
    note: "ADR-009 (lines 162-166) motivates per-epoch shard rotation"
  - target: pillar-block-lattice
    type: supports
    weight: 0.7
    note: "The ADR family is the non-shielded design baseline"
related: []
source_url: "repo:README.md"
---

# Source: README ADRs

`README.md`'s **Architecture Design Decisions** section (lines 102–173) records ADR-001 through ADR-010, the design baseline of the non-shielded consensus stack:

- **ADR-001** trait-based abstraction over inheritance (all pluggable components are traits)
- **ADR-002** DashMap for concurrent registry state
- **ADR-003** deterministic leader election via `SHA-256(epoch)`-seeded RNG over a sorted stake set
- **ADR-004** deterministic per-transaction routing (tx-id-seeded selection over a frozen stake snapshot)
- **ADR-005/010** optimistic finality with the 2/3 quorum repurposed as a conflict-only dispute mechanism
- **ADR-006** stake snapshots in DataProvider, not blocks
- **ADR-007** sentinel as routing authority, not a consensus node
- **ADR-008** conflict = same `(token_id, previous_hash)`, different `current_hash`; higher-stake wins, same-proposer double-sign → slash both
- **ADR-009** executor sharding with per-epoch Fisher-Yates rotation

Caveat: the rest of the README (Quick Start test counts, workspace layout) predates the shielded/ZK/RNS era and is stale; CLAUDE.md's 2026-09-20 note directs readers to the vault for current state.
