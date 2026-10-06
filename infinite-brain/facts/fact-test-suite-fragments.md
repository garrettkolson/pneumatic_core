---
id: fact-test-suite-fragments
title: "Test baseline 2026-10-05 (mesh fragments): 1132 passed / 37 ignored / 0 failed"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/05/2026 workspace baseline after the signed mesh-fragment path: 1132/37/0 (+14) — 8 core fragment tests, 2 generated-cluster end-to-end, 3 probe CLI, 1 judge fix pinned. Adding a field to `Config` broke 18 exhaustive literals in test fixtures across 12 files."
auto_inject: false
applicable_when: "Comparing a test count after a change, or checking the regression baseline"
confidence: 1.0
verified_at: "10/05/2026"
verified_by: "dsh-agent"
staleness_signal: "SUPERSEDED by the next recorded baseline. Otherwise: any change that adds/removes tests."
tags: [fact, tests, baseline, fragments, probe, deployment]
edges:
  - target: fact-mesh-verification-probe
    type: supports
    weight: 0.9
    note: "13 of the 14 new tests come from the fragment path"
  - target: fact-test-suite-mesh-probe
    type: related_to
    weight: 0.9
    note: "Supersedes the 1118/37/0 baseline"
related: []
source_url: "Empty"
---

# Test baseline 2026-10-05 (mesh fragments): 1132 / 37 / 0

Measured 10/05/2026 with `cargo test --workspace`. **1132 passed / 37 ignored /
0 failed**, superseding 1118/37/0. Ignored unchanged at 37.

| Where | Count | What |
|---|---|---|
| `src/node/registry/tests/fragment.rs` | 8 | Dump reports the registry's peers and verifies under its own key; **unvouched (learned) vs registered peers are distinguishable**; a tampered bucket fails; **a re-stamped timestamp cannot revive a signed fragment** (provable staleness); verification uses the consumer's key, not the claimed one; unsigned fragments never reach disk; **two dumps of one state are byte-identical** (DashMap order must not reach the signature); bucket names |
| `testnet-gen/tests/fragments.rs` | 2 | The real chain: generated `config.json` → `Config::build()` → real `NodeRegistry` → `dump_to` → `Fragments::load` → complete at 12/12 edges; then the same files aged (all stale), tampered (bad signature), foreign (unknown reporter), and re-signed with 42 delivery failures (unreachable, node drops from `verified`, and no finding at threshold 100). Plus `bucket_key` ≡ `Role::plural()` |
| `testnet-gen/tests/probe_cli.rs` | 3 | Empty fragment directory ⇒ every node unobserved + PARTIAL + exit 1; two evidence sources ⇒ exit 2 usage fault; stale directory ⇒ rejected evidence printed + PARTIAL |
| `testnet-gen/src/probe.rs` | 1 | `expected_edges` is the **topology's** total even when nobody reports — previously an unobserved fleet advertised 0 edges to check |

## The cost of a public struct field

Adding `mesh_fragment_path` to `Config` broke **18 exhaustive `Config { … }`
literals in 12 files** — test fixtures in root `tests/`, `committer`, `sentinel`,
`finalizer`, `executor`, plus two production-adjacent helpers. All were
`Config {}` literals rather than builders, so the compiler found every one. Cost:
one sweep. Benefit: no fixture silently boots a node that writes files into the
working tree — each site sets `None` explicitly.

Worth knowing before the next `Config` field: the fix is mechanical, but it touches
five crates, and `cargo check -p <crate>` will not reveal them — only a
workspace-wide build does.
