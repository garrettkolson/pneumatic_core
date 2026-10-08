---
id: decision-transaction-ingress-http-edge
title: "ADR-020: Transaction ingress is a bounded HTTP edge on the node, carrying the existing Process envelope — no wire change, no new role"
type: decision
namespace: pneumatic
visibility: namespace
summary: "LANDED 10/07/2026 (roadmap Phase 1). The client surface is `POST /v1/transactions` on the composite node: a hand-rolled raw-tokio responder (the `telemetry.rs` shape — no HTTP crate) whose body is the EXISTING inner `Process` envelope (rmp `Message{action:\"Process\", body: rmp(Transaction), signature, public_key}`). HTTP is a transport adapter in front of the same pipeline entry a peer's gossip uses — zero new wire types, so nothing is lockstep-broken. Ingress re-verifies BOTH signatures at the edge (the sentinel C3 gate: `tx.verify_sender_signature()` over the canonical bytes, plus the envelope signature under `public_key`, plus the C3 binding `public_key == tx.sender`) and refuses before dispatch. Dispatch is injected (`SubmitSink`): the node-server sink wraps the inner envelope in a node-identity-signed outer `\"Verify\"` and calls the same `RoleDispatcher` the RNS bridge uses — from the dispatcher down an HTTP submission is indistinguishable from a peer submission. No Sentinel installed ⇒ `UnknownAction` ⇒ 503, never a silent 200. No new role. Rate limiting deferred to Phase 4; until it exists `PNEUMATIC_INGRESS_ADDR` stays loopback-pinned."
auto_inject: false
applicable_when: "Adding client-facing surfaces (status queries, gateways), touching src/ingress.rs or node-server ingress_sink, or deciding whether the wire needs a client message type"
confidence: 0.95
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If an HTTP crate enters the dependency tree for this surface; if a client-facing message/action type is invented (re-introducing the wire-change this ADR refuses); if the ingress stops verifying either signature at the edge; if rate limiting lands and the loopback-pinning caveat can be retired; if Phase 2 gateways front this surface with different auth"
tags: [adr, design-decision, ingress, http, client, phase-1, security, wire-compatibility]
edges:
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.95
    note: "The Phase-1 pickup card this ADR answers — the decision-first commit it demanded, recorded here"
  - target: fact-committer-commit-was-cache-only
    type: depends_on
    weight: 0.95
    note: "The exit test this ADR's ingress feeds is 'committed block readable through the data service' — unsatisfiable until the committer actually persisted what it committed"
  - target: decision-sentinel-routing-authority
    type: related_to
    weight: 0.85
    note: "Ingress dispatches INTO the sentinel's Verify path rather than becoming a routing authority itself; the C3 gate it mirrors is the sentinel's"
  - target: decision-optimistic-finality
    type: related_to
    weight: 0.7
    note: "Acceptance (200) is not commitment: the ingress answers when the pipeline synchronously accepts; commit is observed through the data service"
  - target: fact-testnet-generator
    type: related_to
    weight: 0.6
    note: "Gateways that would front this surface are testnet-gen topology decisions (Phase 2), not ingress code — the pickup card's trap"
related: []
source_url: "repo:src/ingress.rs, repo:node-server/src/node_server/ingress_sink.rs, repo:client/src/lib.rs"
---

# ADR-020: Transaction ingress — bounded HTTP edge, existing envelope, no wire change

**Status:** accepted, landed 10/07/2026 (`src/ingress.rs`, `node-server/src/node_server/ingress_sink.rs`, `client/`). Roadmap Phase 1 (ADR number as planned in the pickup-card work; supersedes no earlier ADR).

## The decision, and the one the pickup card asked for

The Phase-1 pickup card demanded the *decision* land before any handler: which surface, and whether ingress is a sentinel or a new role. The answer:

- **Surface: HTTP `POST /v1/transactions`**, served by a hand-rolled raw-`tokio` responder — the `telemetry.rs` precedent, not an HTTP crate. The precedent exists, carries zero new dependencies, and its bounds (bounded head, bounded body, read timeout, `Connection: close`) are already load-bearing in production. Health was GET-only; ingress extends the shape with a POST path and the bounds a hostile payload needs: 1 MiB body cap enforced from the header (never buffering what a client merely declared), 8 KiB head cap, 5 s socket read timeout, and **drain-before-respond** (an early 4xx/5xx that leaves a declared body unread makes the OS send an RST that swallows the response — the mutation-style draining test in `client` pins the client half, the draining test pins the server half).
- **Wire: no new type.** The POST body is the *existing* inner `Process` envelope — the exact rmp `Message{action:"Process", body: rmp(Transaction), signature, public_key}` a submitting peer relays on the RNS wire. HTTP here is a transport adapter in front of the pipeline entry the nodes already speak. A Phase-2 gateway can relay the identical artifact.
- **Role: neither.** Ingress is a module on the composite node, not a network role. It never dispatches: the injected `SubmitSink` is the seam, and the node-server's sink wraps the inner envelope in a node-identity-signed outer `"Verify"` and calls `NodeServer::dispatch` — the same `RoleDispatcher` the RNS bridge's `route_data_plane` uses. A node without a Sentinel plugin answers **503** (`UnknownAction` → honest refusal), never a 200 to work it cannot do.
- **Auth at the edge:** ingress re-verifies both signatures — `tx.verify_sender_signature()` (the C3 gate over the canonical bytes) and the envelope signature over the body under `public_key` — and enforces `public_key == tx.sender`. The sentinel re-checks the same facts downstream; ingress adds a refusal earlier, never a weaker one. This is strictly *stronger* than the RNS path, which never authenticated the outer envelope before the dispatcher.

## Why not the alternatives

- **Framed MsgPack endpoint (the wire's own protocol):** reuses the framing but builds a bespoke client protocol where HTTP gives operators curl, JSON errors, and standard ops tooling; and a gateway would speak neither better. The payload is *already* the wire artifact — the framing bought nothing the transport adapter could not.
- **A new client-facing wire type:** the pickup card's own warning — inventing a message type is a wire change, and rmp (named-maps since 10/01, `rmp_serde::to_vec_named`) still means every participant must agree on the schema. Reusing `Process` makes drift structurally impossible: there is one artifact.
- **A new ingress role:** a role exists to participate in role-selected work with stake-gated actions. Ingress holds no state and votes in nothing; a new role would multiply registration, capacity, and fan-out cost for a transport adapter.

## Consequences accepted

1. **Acceptance ≠ commitment.** 200 means the pipeline synchronously accepted; the commit is observed through the data service (that is the Phase-1 exit shape, and the E2E asserts it that way).
2. **No rate limiting until Phase 4** — `PNEUMATIC_INGRESS_ADDR` is explicit opt-in and the runbook pins it to loopback until Phase 4 lands. Per-submission hybrid signature verification (×2 edge + re-check downstream) is expensive by design — the same cost the sentinel's C3 gate already pays; a Phase-4 limiter bounds how many times.
3. **No tx-status endpoint.** Deliberate: the read path for committed effects is the data service, not the ingress.
4. The client crate (`pneumatic_client` + `pneumatic-tx --repeat N`) shares the wire types rather than mirroring a spec, so client/node drift fails CI, not the rollout. The E2E (`client_transaction_through_the_ingress_commits_observable_through_the_data_service`) proves the byte-compatibility claim through a real socket, real ingress, real four-role dispatch, real data service.
