---
id: source-tasks-md
title: "Source: TASKS.md (C#→Rust porting checklist)"
type: source
namespace: pneumatic
visibility: namespace
summary: "The original C#→Rust porting checklist: all 270 checkbox items are [x] complete, incl. SA_01–SA_06 security-audit fixes; the only open work is the 'Remaining test gaps' tail section."
auto_inject: false
applicable_when: "Looking up what the porting/security-remediation phase covered, or finding the last-listed open test gaps"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when TASKS.md is edited"
tags: [tasks, checklist, source, porting, security-audit]
edges:
  - target: source-audit-checklist
    type: related_to
    weight: 0.9
    note: "Same work-tracking lineage; the audit checklist superseded it for new remediation"
  - target: fact-test-suite
    type: related_to
    weight: 0.7
    note: "Its 'Remaining test gaps' tail defines the last open test work"
related: []
source_url: "repo:TASKS.md"
---

# Source: TASKS.md

`TASKS.md` (914 lines) is the original C#→Rust implementation checklist. Every one of its 270 checkbox items is marked `[x]` complete — including the porting phases and all six security-audit fixes (SA_01–SA_06, e.g. SA_04 HKDF + random AES nonce, fixed 2026-08-11) — so there are **no open checkbox items** in the file.

The only open work it documents is the **"Remaining test gaps"** tail section (lines 912–914), which lists: `data.rs` DefaultDataProvider wire-format tests, `server.rs` async poison test, `epoch.rs` Stub* unit tests, `node/registry.rs` send_to_all, e2e pipeline integration (sentinel → executor → finalizer → committer), and `config.rs` loading/parsing helpers.

Use this file to establish *what is done* in the legacy/non-shielded scope; use the audit checklist for the remediation work that came after.
