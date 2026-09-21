---
id: task-e2e-pipeline-integration-test
title: "Open gap: e2e pipeline integration test (sentinel → executor → finalizer → committer)"
type: task
namespace: pneumatic
visibility: namespace
summary: "Open item in TASKS.md's 'Remaining test gaps': a full-pipeline integration test, sentinel → executor → finalizer → committer; also the audit's overall Done-when scenario."
auto_inject: false
applicable_when: "Prioritizing non-shielded test work, or closing the audit's final Done-when criterion"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Close when the integration test lands and the TASKS.md gap list is edited"
tags: [task, testing, integration, pipeline, open-gap]
edges:
  - target: source-tasks-md
    type: derived_from
    weight: 1.0
    note: "Listed in 'Remaining test gaps', lines 912-913"
  - target: source-audit-checklist
    type: related_to
    weight: 0.8
    note: "Done-when #3 (lines 1459-1460) requires a multi-process e2e over the real wire"
  - target: concept-transaction-lifecycle
    type: related_to
    weight: 0.8
    note: "The test exercises the whole state machine"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.7
    note: "Validates the commit/finalize path on the block lattice"
related: []
source_url: "repo:TASKS.md"
---

# Task: e2e pipeline integration test

`TASKS.md` §"Remaining test gaps" (lines 912–913) lists the **e2e pipeline integration test** (sentinel → executor → finalizer → committer) as an open test gap, and it is the most consequential one: it is the only listed gap that exercises the entire transaction pipeline as one unit.

It is also the closing criterion of the audit remediation: AUDIT_CHECKLIST.md's overall **Done-when** (lines 1455–1460) requires "a clean multi-process (≥ 2 nodes per role) run completes a transaction end-to-end over the real wire path — the scenario the audit found inoperable." So this single test is the last piece standing between the current state and the audit's declared finish line.

The shielded S6.1 pipeline test (`tests/shielded_pipeline.rs`) follows the same fixture conventions as `committer/tests/pipeline_integration.rs`, so landing the public-pipeline version first also de-risks the shielded e2e work.
