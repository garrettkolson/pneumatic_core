---
id: log-organize-vault-20260925-135158
type: log
operation: organize-vault
date: 2026-09-25T13:51:58
namespace: pneumatic
summary: "Corrected the transport-layer record after the e2e-pipeline scoping nearly missed it: RNS is the production inter-node wire (RnsConnection is the only production Connection impl; TCP/UDS conns is legacy/local). Documented the 481 B direct-packet gap in RnsConnection and the executor→finalizer 'Execute' vs 'Sign' seam as the two blockers for the e2e pipeline task."
affected_nodes: ["concept-rns-transport", "concept-conn-abstraction", "task-e2e-pipeline-integration-test", "_system/INDEX"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — RNS-is-the-production-wire correction + e2e pipeline design

Scoping the e2e pipeline integration test (the audit's final Done-when) initially targeted the TCP/UDS `conns` layer for the test wire — a mistake, because the production inter-node wire is **RNS**. Verified against code:

- `RnsConnection` (src/rns/conn.rs) is the **only** production `Connection` implementation; every other impl is a test double. The NodeRegistry registration path (node/registry/registration.rs:284,418,554) creates it for every peer.
- The composite bridges RNS `on_packet` → `NetworkPacket` → control/data plane (node-server/build.rs:219-238); roles' `send_to_all` flows over it.
- The TCP/UDS `conns` family (ConnFactory, Sender/Stream/Listener) is the legacy/local layer (data-service channels, original design).

Two production blockers surfaced for the e2e test:

1. **Wire MTU**: `RnsConnection::send` → `send_to` uses the direct packet path only (~481 B plaintext cap); a real `Message` is ~3.8 KB (PQC hybrid signature), so every real pipeline message fails over the wire as wired. Fix: size-based auto-routing to `send_resource_to` (consistent with audit 7.1).
2. **Executor→finalizer seam**: the executor emits `"Execute"`/`ExecutionResult`, but the finalizer's C1 intake expects `"Sign"`/`TransactionSignature` (inner signature over `result_hash`); `"Execute"` is not in `FINALIZER_ACTIONS` (node_server.rs:49), so composites reject it as `UnknownAction`. Fix: executor signs `result_hash` and emits a `Sign` vote.

Updated: `concept-rns-transport` (production-wire status, corrected the "wrapper routes large payloads" claim, both blockers, test-topology pattern), `concept-conn-abstraction` (legacy/local scope note), `task-e2e-pipeline-integration-test` (full design: 8 RNS nodes ≥2/role, flow under test, fixture requirements, Cargo.toml dev-dep), `_system/INDEX` (two row summaries).
