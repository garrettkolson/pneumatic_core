---
id: fact-workspace-layout
title: "Workspace layout: 7 crates, 25 root modules"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Rust workspace: root pneumatic_core lib (25 pub-mod modules in lib.rs incl. rns, shielded, telemetry) + sentinel, executor, finalizer, committer, node-server, prover crates; committer and node-server carry the two binaries."
auto_inject: false
applicable_when: "Locating code, understanding crate boundaries, or scoping a change"
confidence: 1.0
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "If lib.rs module count changes or a crate is added/removed from the workspace"
tags: [workspace, crates, layout, rust]
edges:
  - target: pillar-block-lattice
    type: supports
    weight: 0.9
    note: "The role pipeline maps onto these crates"
  - target: fact-shielded-stack
    type: supports
    weight: 0.9
    note: "Shielded code lives in the root crate's shielded/ module"
  - target: fact-test-suite
    type: supports
    weight: 0.8
    note: "Test counts are per-crate in this layout"
  - target: concept-telemetry-health-metrics
    type: supports
    weight: 0.7
    note: "telemetry is the newest root module (Phase 8); lib.rs also gained the crate-level overview doc"
related: []
source_url: "Empty"
---

# Workspace layout: 7 crates, 25 root modules

The repo is a Rust workspace with seven crates:

- **pneumatic_core** (root) — the protocol library; `src/lib.rs` declares 25
  `pub mod` modules (verified 10/01/2026 by count of declarations) including
  `rns` (Reticulum transport), `shielded` (ZK stack), and `telemetry` (Phase 8
  observability), plus `blocks`, `tokens`, `epoch`, `node`, `conns`, `crypto`,
  `validation`, `transactions`, etc. An earlier version of this fact said 27 —
  that count predates verification against the declaration list.
- **executor, finalizer, committer, sentinel** — the four worker-role crates.
- **node-server** — composite runtime hosting the four roles as plugins.
- **prover** — proving-side crate (client-side proving support).

The root crate is a library (no binary target). The two shipped binaries are
`pneumatic_committer` (committer crate) and `node-server` (node-server crate,
`src/bin/node-server.rs` — the real composite boot landed 10/01/2026 with
Phase 8; before that it was a scaffold stub).
