---
id: fact-workspace-layout
title: "Workspace layout: 7 crates, 27 root modules"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Rust workspace: root pneumatic_core lib (27 modules in lib.rs incl. rns, shielded) + sentinel, executor, finalizer, committer, node-server, prover crates."
auto_inject: false
applicable_when: "Locating code, understanding crate boundaries, or scoping a change"
confidence: 1.0
verified_at: "09/20/2026"
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
related: []
source_url: "Empty"
---

# Workspace layout: 7 crates, 27 root modules

The repo is a Rust workspace with seven crates:

- **pneumatic_core** (root) — the protocol library; `src/lib.rs` declares 27 modules including `rns` (Reticulum transport) and `shielded` (ZK stack), plus `blocks`, `tokens`, `epoch`, `node`, `conns`, `crypto`, `validation`, `transactions`, etc.
- **executor, finalizer, committer, sentinel** — the four worker-role crates.
- **node-server** — composite runtime hosting the four roles as plugins.
- **prover** — proving-side crate (client-side proving support).

The root crate is a library (no binary target).
