---
id: question-constrained-proving-ux
title: "Client-side proving performance on constrained devices (wallet UX)"
type: question
namespace: pneumatic
visibility: namespace
summary: "Open: S6.4 will measure client proving time to decide if client-side Halo2 proving is acceptable on the intended wallet hardware class; seconds-scale proving may be too slow."
auto_inject: false
applicable_when: "Planning S6.4, designing wallet/prover UX, or reviewing circuit-size tradeoffs"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Resolve (delete/answer) when S6.4 benchmarks are recorded and the roadmap records a hardware decision"
tags: [open-question, proving, performance, wallet, ux]
edges:
  - target: source-shielded-roadmap
    type: derived_from
    weight: 1.0
    note: "Part-6 open problem: proving perf on constrained devices"
  - target: decision-client-side-proving
    type: depends_on
    weight: 0.9
    note: "The architecture decision this question puts to the test"
  - target: concept-action-circuit
    type: related_to
    weight: 0.8
    note: "Circuit size at final shape sets the proving time"
  - target: source-shielded-plan
    type: related_to
    weight: 0.7
    note: "S6.4 (lines 988-997) specifies the measurement; <100ms verify budget"
related: []
source_url: "repo:plans/pneumatic-shielded-roadmap.md"
---

# Client-side proving performance on constrained devices

The roadmap's Part-6 open problems and the implementation plan's open question 3 (lines 1024–1025) both flag the same unknown: **S6.4's measured prove time determines whether client-side proving is acceptable on the intended hardware class** — confirming seconds-scale proving is tolerable for wallet UX is explicitly "an open product question" (impl plan line 993).

Why it's load-bearing: the plan's architecture deliberately skips the Executor and puts proving on the client (roadmap Part 2.1, locked decision). If a constrained wallet device (phone-class hardware, not a dev workstation) can't produce a proof in reasonable time, the design's central premise needs a fallback — e.g., a proving service, a cheaper circuit, or async/deferred submission.

The measurement is specified in **S6.4 — Proving/verification benchmarks** (impl plan lines 988–997): bench-gated (excluded from the default suite, like the live-prove tests), measuring client prove time *and* network verify time; verification is asserted **< 100 ms** — if the circuit blows that, it's a circuit-design finding to escalate, "not a silent accept." The recorded numbers then report back to the roadmap owner to close this question.
