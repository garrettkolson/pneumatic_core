---
id: log-implement-code-20261007-180500
type: log
operation: implement-code
date: "2026-10-07T18:05:00"
namespace: pneumatic
summary: "Closed roadmap Phase 1 (transaction ingress + client) the way the pickup card ordered it: the **decision first** (ADR-020 — raw-tokio HTTP edge on the composite, POST body = the existing inner `Process` envelope, no client-facing wire type, no new role, both signatures re-verified at the edge, dispatch injected into the same `RoleDispatcher` the RNS bridge uses). Then the exit test in the Phase-0 shape — real client → real ingress socket → four-role composite → **real data service**, result hash recomputed outside the pipeline — which was **unsatisfiable as written**: no committed block had ever been observable through the data service, because `commit_block` appended to the committer's cache only, and that cache was empty at every boot (`distribute_token` has no production caller). Fixed both (warm-on-miss through the provider; persist inside the `get_mut` guard; new `TokenPersist` error), fixture wired to production parity after the fixture's split-provider split was caught by its own new failure. Three mutants run — remove persist, disable warm, bypass the envelope-signature check — the last exposed a genuine coverage gap mid-mutation, closed by a new 401 test that fails the mutant. Suite 1156/37/0 → **1179/37/0**; corrected the pickup card's stale 'rmp encodes positionally' claim (named maps since 10/01, `encoding.rs:17`)."
affected_nodes: ["task-multihost-testnet-rollout", "decision-transaction-ingress-http-edge", "fact-committer-commit-was-cache-only", "repo:src/ingress.rs", "repo:node-server/src/node_server/ingress_sink.rs", "repo:client/src/lib.rs", "repo:client/src/main.rs", "repo:committer/src/block_services.rs", "repo:committer/src/committer/tests/commit.rs", "repo:committer/src/committer/tests/helpers.rs", "repo:committer/tests/pipeline_integration.rs", "repo:node-server/src/node_server/tests/e2e.rs", "repo:node-server/src/bin/node-server.rs"]
tags: ["log", "implement-code", "phase-1", "ingress", "adr-020", "client", "http", "committer", "persistence", "data-service", "exit-test", "mutation-verified", "test-baseline"]
---

Phase 1's pickup card asked for the decision before the server. It got it — and then the
exit test the card itself specified turned out to describe a system that could not exist
yet. That is what this log is about: the order mattered.

## The decision (ADR-020)

`POST /v1/transactions` on the composite node, answered by a hand-rolled raw-`tokio`
responder (the `telemetry.rs` precedent; no HTTP crate), whose body is the **existing**
inner `Process` envelope — `Message{action:"Process", body: rmp(Transaction), signature,
public_key}` — byte-identical to what a submitting peer relays on the RNS wire. No client
message type was invented (the card's wire-change warning), no new role: ingress is a
module, and dispatch is an injected `SubmitSink`. The node-server sink wraps the inner
envelope in a node-identity-signed outer `"Verify"` and calls the same dispatcher the RNS
bridge calls. No Sentinel installed ⇒ `UnknownAction` ⇒ **503** — never a 200 to work the
node cannot do. Both signatures (sender-over-canonical-bytes, envelope-over-body) plus
the C3 binding are re-verified at the edge: strictly stronger than the RNS path, which
never authenticated the outer envelope before the dispatcher. Rate limiting is Phase 4's;
`PNEUMATIC_INGRESS_ADDR` stays loopback-pinned until then.

## The exit test, and the hole under it

`client_transaction_through_the_ingress_commits_observable_through_the_data_service`
boots a real `pneumatic_data_service`, points the production `DefaultDataProvider` at it,
applies real genesis, seeds the token and user **through the provider over the socket**,
boots the four-role composite with its committer token cache **empty**, submits one
transaction from `pneumatic_client` over a real TCP socket, and reads the committed chain
back through a **fresh** provider — the tip hash checked against a result the Transfer
engine computed outside the pipeline.

To make that test pass, two live-path defects had to be fixed first (`fact-committer-commit-was-cache-only`):

- `BlockServices::commit_block` was cache-only: a committed block never reached the data
  service, and ADR-019's per-token selection salt was silently pinned at each token's
  genesis tip forever.
- The committer cache was empty at every boot (`distribute_token` has no production
  caller), so a fresh committer answered every `Commit` with `TokenNotFound` while the
  store held the token.

Fix: warm the cache on miss from the provider (fail-closed error now names the store);
persist the advanced token **inside the `get_mut` guard** (outside it, two commits could
reorder save-over-append and write stale state); a failed save is a new
`CommitterError::TokenPersist`, not a success report. The committer test fixtures then
themselves broke — they had been wiring `BlockServices` a **second**, network-dialing
`DefaultDataProvider` while the `Committer` held the in-memory one; production injects one
provider into both. Aligning the fixture to production parity is part of the fix: the
failure was the fixture finally being honest.

## How claims were checked

- Every "the code does X" claim read at the source before writing (warm-miss path,
  no-caller grep for `distribute_token`, `build.rs:177`/`main.rs:321` single-provider
  wiring, `encoding.rs:17` named maps).
- **Mutation-verified, three ways**: removing `save_token` fails the persistence unit
  test, the cold-warm test, and the E2E (at `left: 1` — genesis only); disabling the warm
  fails both cold-cache tests and the E2E; bypassing the edge envelope-signature check
  was NOT caught — the "alien key" test trips the C3 binding before reaching that check.
  New test `an_envelope_signature_that_fails_under_the_bound_key_is_401` isolates it and
  fails the mutant. Every mutant restored and its suite re-greened afterwards.
- One protocol bug found the honest way: an early-response path that left a declared body
  unread made the kernel RST the response (draining test caught it red on first run);
  responder now drains the bounded body before any refusal.
- Full workspace `--no-fail-fast`: **1179/37/0** (from 1156/37/0; +23 tests, all new,
  ignored count unchanged).

## Deliberately not done

Whole-`Token` re-serialization per commit (delta persistence is future work); no
tx-status endpoint (the data service is the read path); rate limiting deferred to
Phase 4 as planned; no gateway — Phase 2's topology decision, per the pickup card's trap;
the sustained multi-host run through `pneumatic-tx --repeat N` belongs to Phase 2's
transport exercise, which now has a way to generate traffic.
