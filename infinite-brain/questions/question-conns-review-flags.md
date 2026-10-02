---
id: question-conns-review-flags
title: "Conns-layer review flags from the Phase 8 rustdoc pass (dead error variants, unused HEARTBEAT_PORT, read_exact asymmetry)"
type: question
namespace: pneumatic
visibility: namespace
summary: "Documenting src/conns surfaced three likely defects: ConnError::CouldNotEstablishStream and ConnectionRejectedByRemote are never constructed; HEARTBEAT_PORT (42000) is declared but bound nowhere; sync Stream::read_exact returns Result<(), Error> while async StreamReader::read_exact returns Result<usize, ConnError>."
auto_inject: false
applicable_when: "Touching the conns trait family, the heartbeat path, or the error enum"
confidence: 0.85
verified_at: "10/01/2026"
verified_by: "dsh-agent (rustdoc-pass agent report, greps spot-checked)"
staleness_signal: "Resolved when the variants are wired-or-removed, HEARTBEAT_PORT is bound-or-deleted, and the read_exact signatures are unified-or-documented-by-design"
tags: [conns, review-flags, dead-code, api-asymmetry, phase-8]
edges:
  - target: concept-conn-abstraction
    type: related_to
    weight: 0.9
    note: "All three flags sit inside the Connection/Sender/Stream/Listener trait families this concept describes"
  - target: event-phase8-production-readiness
    type: derived_from
    weight: 0.7
    note: "Surfaced while documenting the conns layer for Phase 8 rustdoc"
related: []
source_url: "Empty"
---

# Conns-layer review flags (from the Phase 8 doc pass)

Three findings a rustdoc pass surfaced; each needs an owner decision (wire it,
delete it, or document it as intentional):

1. **Dead `ConnError` variants** — `CouldNotEstablishStream` and
   `ConnectionRejectedByRemote` are never constructed anywhere in the workspace.
   Dead error variants mislead readers about real failure modes; prefer removal
   (they are never matched exhaustively per the doc pass) over keeping them
   "for the future".
2. **`HEARTBEAT_PORT = 42000`** — declared in `src/conns.rs` but referenced
   nowhere; the heartbeat logic in the node-registry layer binds no port. Either
   a forgotten wiring (a heartbeat channel that was specced and never built) or
   dead config. Doc'd as "reserved, not currently bound".
3. **`read_exact` return-type asymmetry** — the sync `Stream::read_exact`
   returns `Result<(), Error>` while async `StreamReader::read_exact` returns
   `Result<usize, ConnError>`. The two traits otherwise mirror each other;
   the mismatch looks unintentional and forces callers to handle byte counts
   only on the async side.

The doc pass also noted `ConnTarget`'s manual `Clone` impl is identical to what
`#[derive(Clone)]` would emit (no found rationale) — cosmetic, not flagged as a
defect.
