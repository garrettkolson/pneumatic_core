---
id: source-readme-adrs
title: "Source: README.md ADRs (ADR-001…ADR-010)"
type: source
namespace: pneumatic
visibility: namespace
summary: "The ADR-001…010 source (formerly README lines 102-173); the section was removed 09/26/2026 when the README was trimmed and the ADRs were promoted to first-class decision nodes in decisions/."
auto_inject: false
applicable_when: "Tracing the provenance of the non-shielded design decisions"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale if a new design decision is recorded only in the README without a vault node"
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

`README.md`'s **Architecture Design Decisions** section (former lines 102–173) recorded ADR-001 through ADR-010, the design baseline of the non-shielded consensus stack. **On 09/26/2026 the section was removed** as part of a README trim (690 → ~250 lines, contributor-focused), and each ADR was promoted to a first-class decision node:

- ADR-001 → `decision-trait-based-abstraction`
- ADR-002 → `decision-dashmap-registries`
- ADR-003 → `decision-deterministic-leader-election`
- ADR-004 → `decision-deterministic-per-tx-routing`
- ADR-005 + ADR-010 → `decision-optimistic-finality` (the README recorded the decision twice)
- ADR-006 → `decision-stake-snapshots-in-dataprovider`
- ADR-007 → `decision-sentinel-routing-authority`
- ADR-008 → `decision-conflict-definition-and-resolution`
- ADR-009 → `decision-executor-sharding`

The README now keeps a one-line-per-ADR summary table and points to `infinite-brain/decisions/`. This source node remains the provenance record for that migration. (The rest of the old README — stale test counts, pre-RNS layout — was rewritten in the same pass; the vault is now the source of truth for current state, per CLAUDE.md's standing protocol.)
