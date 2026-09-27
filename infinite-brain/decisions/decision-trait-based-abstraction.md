---
id: decision-trait-based-abstraction
title: "ADR-001: Trait-based abstraction over inheritance"
type: decision
namespace: pneumatic
visibility: namespace
summary: "All pluggable components are Rust traits with concrete impls (Connection, Sender, Stream, Listener, DataProvider, BlockValidator, Logger, AsymCryptoProvider, HashProvider, IActionRouter) — zero-cost, stub-testable, swappable."
auto_inject: false
applicable_when: "Adding a new pluggable component, test double, or swapping an implementation"
confidence: 0.95
verified_at: "09/26/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when a pluggable seam stops being trait-based (e.g. a component gains concrete-type coupling in its API)"
tags: [adr, design-decision, traits, abstraction, testing]
edges:
  - target: concept-conn-abstraction
    type: supports
    weight: 0.9
    note: "The conns trait families are the canonical instance of this decision"
  - target: concept-data-provider
    type: supports
    weight: 0.8
    note: "DataProvider + StubDataProvider show the test-stub payoff"
  - target: pillar-block-lattice
    type: related_to
    weight: 0.6
    note: "Part of the non-shielded design baseline"
  - target: source-readme-adrs
    type: derived_from
    weight: 0.9
    note: "Migrated verbatim from README ADR-001 when the README was trimmed (09/26/2026)"
related: []
source_url: "repo:README.md (former lines 104-108)"
---

# ADR-001: Trait-based abstraction over inheritance

All pluggable components use Rust traits with concrete implementations rather than inheritance hierarchies: `Connection`, `Sender`, `Stream`, `Listener`, `DataProvider`, `BlockValidator`, `Logger`, `AsymCryptoProvider`, `HashProvider`, `IActionRouter`.

**Rationale**: Rust lacks inheritance; traits provide zero-cost abstractions and allow any concrete type to satisfy an interface. This makes testing straightforward (`StubDataProvider`, `StubLeaderSelector`) and allows swapping implementations without refactoring consumers.
