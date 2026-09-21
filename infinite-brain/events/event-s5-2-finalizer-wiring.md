---
id: event-s5-2-finalizer-wiring
title: "S5.2: finalizer wiring for shielded transactions"
type: event
namespace: pneumatic
visibility: namespace
summary: "Commit 1b7e4f0 (current HEAD, 09/20/2026): 'feat: finalizer wiring for zk-shielded transactions' — the shielded validation spec is now on the finalizer's block path."
auto_inject: false
applicable_when: "Dating shielded features, checking what's integrated, or continuing S5.3"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Historical event — never goes stale"
tags: [event, shielded, finalizer, milestone]
edges:
  - target: pillar-shielded-value-transfer
    type: supports
    weight: 0.9
    note: "Marks S5.2 complete"
  - target: concept-transaction-lifecycle
    type: supports
    weight: 0.9
    note: "The 4-gate spec became part of the block path"
  - target: event-node-server-composite
    type: preceded_by
    weight: 0.7
    note: "Node-server composite runtime predates shielded wiring"
related: []
source_url: "git:1b7e4f0"
---

# S5.2: finalizer wiring for shielded transactions

Commit **1b7e4f0** — "feat: finalizer wiring for zk-shielded transactions" — is the current HEAD (09/20/2026). It integrates the shielded validation path into the **finalizer's** block-processing path: shielded transactions now flow through `ShieldedValidationSpec`'s four fail-closed gates as part of normal block handling.

This was the last of the S5.1–S5.2 phase deliverables. The phase sequence it completes: S1.1 halo2 (16ec89c) → poseidon/notes (626c0a4, 047362d, eb7bede, ec89fd0, 637c8f3, abbb900) → S2.1/2.2 (804ce1b, 7c1ed22) → S3.1–3.3 (c0220fc) → S4.1–4.3 (78a61ba, 9e0b76f, 47fd716) → S5.1 pool-view seam (e43124e) → S5.2 (1b7e4f0).
