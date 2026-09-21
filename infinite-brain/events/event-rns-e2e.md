---
id: event-rns-e2e
title: "RNS end-to-end transport tests landed"
type: event
namespace: pneumatic
visibility: namespace
summary: "Commit 451a00d: end-to-end tests over the Reticulum (RNS) transport layer — messages sent/received through the full rns-net stack with the exact pinned versions."
auto_inject: false
applicable_when: "Checking RNS integration status, transport reliability evidence, or upgrade risk"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Historical event — never goes stale"
tags: [event, rns, transport, testing]
edges:
  - target: fact-rns-pinning
    type: supports
    weight: 0.9
    note: "E2E coverage of the pinned rns stack"
  - target: fact-wire-protocol
    type: supports
    weight: 0.7
    note: "Exercises the 16 MB frame cap path"
related: []
source_url: "git:451a00d"
---

# RNS end-to-end tests

Commit **451a00d** landed **end-to-end tests over the Reticulum Network Stack** transport: messages flow through the full rns-net 0.7.0 stack (via `src/rns/wrapper.rs` and the conn/identity/config modules) between test endpoints. These are the integration tests that make the exact-pinning strategy practical — an rns upgrade must pass e2e before it's acceptable.

Live-network RNS runs remain `#[ignore]`d (benchmark-only); the e2e suite runs in normal CI.
