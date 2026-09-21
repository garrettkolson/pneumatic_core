---
id: task-data-provider-wire-tests
title: "Open gap: DefaultDataProvider wire-format tests (data.rs)"
type: task
namespace: pneumatic
visibility: namespace
summary: "Open gap in TASKS.md's 'Remaining test gaps': wire-format tests for the DefaultDataProvider (data.rs); siblings: server.rs async poison, epoch stubs, registry send_to_all, config helpers."
auto_inject: false
applicable_when: "Prioritizing non-shielded test work on the data-service boundary"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Close when the data.rs tests land and the TASKS.md gap list is edited"
tags: [task, testing, data-provider, wire, open-gap]
edges:
  - target: source-tasks-md
    type: derived_from
    weight: 1.0
    note: "Listed first in 'Remaining test gaps', lines 913-914"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.8
    note: "Pins the MsgPack length-prefixed framing of the data-service channel"
  - target: source-audit-checklist
    type: related_to
    weight: 0.6
    note: "Same wire-integrity lineage as audit Phase 1"
related: []
source_url: "repo:TASKS.md"
---

# Task: DefaultDataProvider wire-format tests

`TASKS.md` §"Remaining test gaps" (lines 913–914) lists **`data.rs` (DefaultDataProvider tests)** as an open gap, with the stated action: "DefaultDataProvider wire format tests" — i.e., round-trip tests for the MsgPack-over-TCP/UDS channel the data provider uses against the local data service.

The same gap section lists four siblings of lower pipeline scope: `server.rs` (async poison test fix), `epoch.rs` (StubEpochReconciler/StubStakingManager unit tests), `node/registry.rs` (`send_to_all`), and `config.rs` (loading/parsing unit tests — "test helpers exist" per line 914). The data-provider item is listed first, and it matters beyond coverage: the stake-snapshot and pool-state persistence patterns (Phase 5.4 attested snapshots, and S5.3's pool save/load `DataOp`s) both ride this boundary, so a regression that corrupts an envelope here would surface far from its cause.
