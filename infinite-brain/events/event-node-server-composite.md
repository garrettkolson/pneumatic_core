---
id: event-node-server-composite
title: "Composite node-server runtime (4 role plugins)"
type: event
namespace: pneumatic
visibility: namespace
summary: "Commit e96a00a: the node-server composite runtime — one process hosting executor, finalizer, committer, and sentinel as plugins for full-node deployments."
auto_inject: false
applicable_when: "Understanding deployment topology, touching node-server, or adding a role plugin"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Historical event — never goes stale"
tags: [event, node-server, runtime, deployment]
edges:
  - target: fact-workspace-layout
    type: supports
    weight: 0.9
    note: "Explains the node-server crate's role in the 7-crate workspace"
  - target: pillar-block-lattice
    type: supports
    weight: 0.8
    note: "The runtime that hosts the 4-role pipeline"
  - target: event-s5-2-finalizer-wiring
    type: followed_by
    weight: 0.7
    note: "Shielded finalizer wiring built on this composite runtime"
related: []
source_url: "git:e96a00a"
---

# Composite node-server runtime

Commit **e96a00a** introduced the **composite node-server runtime**: a single process that hosts the four worker roles — executor, finalizer, committer, sentinel — as **plugins**, rather than requiring four separate processes per full node.

This is the deployment shape for full nodes: one composite process per node, with each role plugin receiving its share of the message stream. It also means a change to any role's wiring (like the S5.2 shielded finalizer wiring) must be validated through the composite runtime's test suite (31 tests in the node-server crate).
