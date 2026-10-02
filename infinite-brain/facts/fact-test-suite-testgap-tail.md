---
id: fact-test-suite-testgap-tail
title: "Test baseline 2026-10-01 (test-gap tail): 1021 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Workspace baseline after closing the TASKS.md 'Remaining test gaps' tail (+24): core lib 685 (wire_format 9, config load/parse 10, epoch stubs 3, server async 2 — rest unchanged from Phase 8), integration + workspace crates as before; 0 failed, 37 ignored."
auto_inject: false
applicable_when: "Comparing test counts after this date, or checking the pre-existing failure baseline"
confidence: 0.95
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Any workspace test run; check against `cargo test --workspace`"
tags: [fact, tests, baseline]
edges:
  - target: fact-test-suite-phase8
    type: derived_from
    weight: 1.0
    note: "Supersedes the 997-passed Phase 8 baseline with the +24 test-gap-tail delta"
  - target: task-data-provider-wire-tests
    type: related_to
    weight: 0.8
    note: "The closed gap task this run measures"
related: []
source_url: "repo:cargo test --workspace output, 10/01/2026"
---

# Test baseline 2026-10-01 (test-gap tail): 1021 / 37 / 0

`cargo test --workspace` after closing the TASKS.md "Remaining test gaps" tail:
**1021 passed / 37 ignored / 0 failed.**

Delta from `fact-test-suite-phase8` (997): **+24, all in `pneumatic_core` lib (661 → 685)**:

| Area | New tests | What they pin |
|---|---|---|
| `data::wire_format_tests` | 9 | Real framed channel (4B BE len + 32B HMAC tag + rmp body) over UDS: get/save shapes, envelope round-trips, tamper → `SnapshotCorrupt`, wrong-key → `PeerUnauthenticated`, garbage → `DeserializationError`, op guards, connect-refusal fail-closed |
| `config::config_tests` | 10 | Path-injected `load_spec_from` / `get_environment_metadata_from`: parse, defaults, all four boot-failure classes, env-dir keyed load, empty-file skip, H6 range rejection |
| `epoch::tests::stubs` | 3 | Stub contracts: empty reconciliation, accept-any ops, `dyn` object safety |
| `server::tests` | 2 | Panicking async job kills its worker loop (later jobs never run); `tokio::sync::Mutex` non-poisoning guard |

Ignored count unchanged (37). `node/registry.rs send_to_all` needed no new tests — Phase 6.2 `fanout.rs` already covers it (the gap list was stale).
