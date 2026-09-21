---
id: pattern-fail-closed
title: "Fail-closed construction"
type: pattern
namespace: pneumatic
visibility: namespace
summary: "Invalid state must be impossible to construct, not merely rejectable: ActionCircuit construction, ShieldedValidationSpec checks, and pool-view seams all fail closed on bad input."
auto_inject: false
applicable_when: "Designing new validation, circuit, or pool code; reviewing error handling"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If new consensus-facing code adopts fail-open behavior, the pattern claim weakens"
tags: [fail-closed, safety, pattern, validation]
edges:
  - target: concept-action-circuit
    type: supports
    weight: 0.9
    note: "Circuit rejects structurally invalid actions at construction"
  - target: concept-transaction-lifecycle
    type: supports
    weight: 0.9
    note: "All 4 validation gates fail closed"
  - target: decision-nullifier-consensus-critical
    type: supports
    weight: 0.8
    note: "Membership check fails closed on registry errors"
related: []
source_url: "Empty"
---

# Fail-closed construction

Consensus-facing code in this repo follows a consistent discipline: **invalid state is impossible to construct or always rejected**, never silently passed. Concrete instances:

- `ActionCircuit` construction rejects structurally invalid actions (bad commitment consistency) *before* proving begins — a bad input fails the build, not the proof.
- `ShieldedValidationSpec` runs all four gates with any error/miss → rejection; a `Shielded` name in the registry without a registered spec is itself a failure (fail-closed at `validation.rs:342`).
- The pool-view seam returns errors rather than inventing state when the underlying pool lacks data.

When adding consensus-path code, match this: prefer `Result` paths where the `Err` side is the only side.
