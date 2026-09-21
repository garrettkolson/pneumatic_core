---
id: pattern-cfg-test-proving
title: "Live-proving tests are #[ignore]d (benchmark-only)"
type: pattern
namespace: pneumatic
visibility: namespace
summary: "Tests that actually generate zk-proofs (or run live RNS) are #[ignore]d: core 5, prover 1, sentinel 1, rns_live 1 — the 8 ignored in the baseline; they are benchmarks, not regression tests."
auto_inject: false
applicable_when: "Adding proving tests, interpreting test baseline counts, or deciding what belongs in CI"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the ignore count in the baseline changes, or a live-prove test is made non-ignored"
tags: [tests, proving, ignore, ci]
edges:
  - target: decision-client-side-proving
    type: derived_from
    weight: 1.0
    note: "Proving is client-side, so proving tests don't belong in the worker baseline"
  - target: fact-test-suite
    type: supports
    weight: 1.0
    note: "Explains the 8 ignored in the 822/8 baseline"
related: []
source_url: "Empty"
---

# Live-proving tests are #[ignore]d

Because proving is client-side (see the decision), tests that generate real proofs are **slow by nature** and are gated with `#[ignore]`:

- pneumatic_core: 5
- prover: 1
- sentinel: 1
- rns_live: 1

Total: **8 ignored** — exactly the ignored count in the 09/20/2026 workspace baseline. They are **benchmark/interop tests** (run deliberately with `--ignored`), not part of the normal regression baseline. New tests that invoke real proof generation or live RNS traffic should follow the same gate so the CI baseline stays fast.
