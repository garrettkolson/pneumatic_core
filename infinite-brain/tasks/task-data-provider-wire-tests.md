---
id: task-data-provider-wire-tests
title: "RESOLVED 10/01/2026: DefaultDataProvider wire-format tests (data.rs) — all five test-gap items closed"
type: task
namespace: pneumatic
visibility: namespace
summary: "CLOSED: wire_format_tests (9) landed in data.rs plus siblings — server.rs async poison (2), epoch stubs (3), config load/parse (10 via path-injected seams); send_to_all was already covered by Phase 6.2 fanout.rs. Suite 997 → 1021 / 37 / 0."
auto_inject: false
applicable_when: "Historical: understanding why the wire_format_tests module exists in data.rs"
confidence: 0.95
verified_at: "10/01/2026"
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

## Resolution (10/01/2026)

All five items in the gap list are closed; see `fact-test-suite-testgap-tail` for the measured baseline (1021 / 37 / 0) and TASKS.md §"Remaining test gaps — COMPLETED 2026-10-01" for the item-by-item record. Highlights: the wire suite talks to an in-process fake data service over UDS speaking the real `[len][HMAC tag || rmp]` framing — including envelope tamper-rejection and HMAC wrong-key rejection across a real socket; `config.rs` gained path-injected load seams (`load_spec_from` / `get_environment_metadata_from`) so the real `config.json` + `/env` boot parsing is unit-tested without touching process state; the async-poison gap turned out to be a semantic mismatch (`tokio::sync::Mutex` cannot poison) and is now covered by a worker-death test plus an assumption guard. `send_to_all` required nothing new — Phase 6.2 `fanout.rs` already tested it; the gap list was stale.
