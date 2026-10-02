---
id: fact-test-suite-phase8
title: "Test baseline 2026-10-01 (post-Phase-8): 997 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Post-Phase-8 workspace baseline: 997 passed / 37 ignored / 0 failed (+9 over the 988 P10 baseline: 7 telemetry unit tests + 2 deploy-example guard tests)."
auto_inject: false
applicable_when: "Quoting the current test baseline or regressing after a change"
confidence: 1.0
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale whenever the workspace test counts change — re-run cargo test --workspace"
tags: [tests, baseline, ci, quality, phase-8]
edges:
  - target: fact-test-suite
    type: derived_from
    weight: 1.0
    note: "Supersedes the 988-passed same-day P10 baseline with the +9 Phase 8 delta"
  - target: event-phase8-production-readiness
    type: related_to
    weight: 0.9
    note: "The delivery this baseline verifies"
related: []
source_url: "Empty"
---

# Test baseline 2026-10-01 (post-Phase-8)

`cargo test --workspace` on 10/01/2026, after Phase 8: **997 passed, 37 ignored,
0 failed**.

Delta vs the 988 P10 baseline: **+9** — 7 in `src/telemetry.rs` (metrics
rendering/reads, health 200→503, /metrics body, 404/405 paths, silent-client
survival, health-JSON shape) and 2 in `tests/deploy_examples.rs` (the
`deploy/config/**` examples must parse as `ConfigSpec` and parse+validate+load
as `EnvironmentMetadataSpec`).

Ignored count unchanged at 37 (live-proving/live-RNS benchmarks,
`pattern-cfg-test-proving`). Docker image `pneumatic:phase8` built green as
part of the same verification.
