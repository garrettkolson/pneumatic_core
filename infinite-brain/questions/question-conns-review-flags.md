---
id: question-conns-review-flags
title: "RESOLVED 10/01/2026: conns review flags confirmed and fixed (variants removed, HEARTBEAT_PORT deleted, read_exact normalized)"
type: question
namespace: pneumatic
visibility: namespace
summary: "RESOLVED: all three flags were verified as defects and fixed — the two never-constructed ConnError variants and the zero-reference HEARTBEAT_PORT were deleted, and StreamReader::read_exact was normalized to Result<(), ConnError> (its usize was always buffer.len()); suite stayed 997/0/37."
auto_inject: false
applicable_when: "Historical: why the ConnError enum is smaller than the C# original and why StreamReader::read_exact returns unit"
confidence: 1.0
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "Resolved; retained as the historical record per the no-delete rule"
tags: [conns, review-flags, dead-code, api-asymmetry, phase-8, resolved]
edges:
  - target: concept-conn-abstraction
    type: related_to
    weight: 0.9
    note: "All three flags sat inside the Connection/Sender/Stream/Listener trait families this concept describes"
  - target: event-phase8-production-readiness
    type: derived_from
    weight: 0.7
    note: "Surfaced while documenting the conns layer for Phase 8 rustdoc; fixed in the follow-up commit"
related: []
source_url: "Empty"
---

# Conns-layer review flags (from the Phase 8 doc pass) — RESOLVED

All three were verified against the code (not just the doc-agent report) and
confirmed defects; all three fixed 10/01/2026:

1. **Dead `ConnError` variants — REMOVED.** `CouldNotEstablishStream` and
   `ConnectionRejectedByRemote` were never constructed since the initial commit
   (git pickaxe-confirmed; vestigial from the C# port). Safe to delete: the
   only other matches were `matches!` test asserts, `data.rs`'s catch-all
   mapping, and `errors.rs`'s `to_string()` conversion. Both variants, their
   docs, and their `Debug`/`Display` arms are gone.
2. **`HEARTBEAT_PORT = 42000` — DELETED.** Zero references workspace-wide;
   heartbeats run over the RNS control plane as binding-signed messages
   (`node/registry/heartbeat.rs`), no component ever bound the port. A relic
   of the pre-RNS port-per-protocol design.
3. **`read_exact` asymmetry — NORMALIZED.** Verified the `usize` could carry no
   information (tokio's `read_exact` reports `Ok(buffer.len())` only on a
   completed fill; EOF-before-fill is the `ReadError` path), and no production
   caller used the count. `StreamReader::read_exact` now returns
   `Result<(), ConnError>`, matching the sync `Stream` trait; the impls drop
   the count explicitly with a comment, and the trait doc states the contract
   ("EOF is an error, never a short-but-successful read — hence no count").

Gate: `cargo check --workspace --all-targets` was the completeness proof (the
compiler found every construction/match/call site); suite stayed
**997 / 37 / 0**. The `ConnTarget` manual-`Clone` observation stayed cosmetic —
unchanged.
