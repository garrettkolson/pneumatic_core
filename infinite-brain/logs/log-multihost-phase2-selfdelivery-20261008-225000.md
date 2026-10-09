---
id: log-multihost-phase2-selfdelivery-20261008-225000
type: log
operation: multihost-phase2-debug + defect-mitigation
date: "2026-10-08T22:50:00"
namespace: pneumatic
summary: "Landed composite self-delivery and PROVED it the way the fact demanded — the 4-container rehearsal grew its chain with no test-side relaying (0 → 5, then 15 txs taking the commit counter 2 → 17, probe 12/12 exit 0, no same-hash double commit on any node). The seam: a fan-out's local copy is wire-framed and fed through the SAME inbound closure the transport uses (self_delivery.rs, installed by the bridge only), exactly-once by two structural guards. Delivery alone was not the fix — sender auth is bucket-derived, so the host's own key had to learn its installed roles or the copy was refused as Unregistered. Pinned by a composite e2e with the harness relay REMOVED; 12 unit tests + 5 mutations; suite 1195 → 1208. Then the exit instrument itself was found wrong (a token's chain is a 5-block sliding window, so the count gate could never pass) and fixed with a negative control; and the next blocker named: per-host fork divergence with no cross-host resolution"
affected_nodes: ["fact-composite-no-self-delivery", "fact-composite-per-host-fork-divergence", "fact-token-chain-window-caps-delivery-gate", "fact-composite-fanout-role-collision", "task-multihost-testnet-rollout", "_system/INDEX.md"]
tags: ["log", "multihost-phase2-debug", "defect-mitigation", "composite", "self-delivery", "fanout", "role-resolution", "rehearsal", "measurement"]
---

Picked up at the bridge-layer self-subscription card. What landed (`2fb44dc`):
`src/node/registry/self_delivery.rs` — `install_self_delivery(roles, sink)` plus
`deliver_self_copy`, called at the head of `send_to_all`, `send_to_all_blocking`
and the keyed path in `send_to_peers_blocking`. Of the two sketched shapes the
sink won: the RNS wrapper has no local-delivery seam (`send_frame` has no route to
our own rhash, and sending there kills the worker per
`fact-rns-bridge-spawn-kills-worker`), while feeding the framed `NetworkPacket`
into `build_runtime`'s `on_packet` closure needs no transport and traverses the
identical parse, control branch, admit-lists and dispatcher. `set_declared_roles`
deliberately does NOT subscribe a host to its own fan-out (a full-node config
declares all four types; a split-deployment host must keep peer-only behavior).
Exactly-once is structural: the host runs the role AND is not already in the
target bucket — the second guard is what keeps the relay-convention fixtures from
receiving both a recording-connection copy and a local copy.

Delivery alone did not grow the chain, and that was worth the detour: every
inbound handler authenticates its sender through a bucket-derived lookup, so a
self copy signed by the host's own identity resolved to zero roles and died as
`Unregistered`. `roles_of_key` now unions the installed self-roles for this host's
own key — true rather than permissive, since an envelope verifying under this
host's key can only come from this host, and the roles are exactly the plugins the
bridge installed. Any other key resolves unchanged, so `Unregistered` stays
reachable. Executor's `NoFinalizers` gate now asks `serves_locally` — a bucket
this host also runs is not empty in the sense that gate means.

Proof, in the demanded order: rebuilt the image (`pneumatic:phase2n`; buildx state
writes are sandbox-blocked, so `DOCKER_BUILDKIT=0 --network=host`), `IMAGE=` made
overridable in `up.sh`, then probe exit 0 / 12 edges and **the chain grew with no
test-side relaying** — 0 → 5 blocks, and later 15 txs moving the commit counter
2 → 17. Only then the pin: `a_composite_pipeline_commits_with_no_test_side_relay`
— production client/ingress/data service, four live roles, NO bucket entry for the
host's own key, one dispatched message. Withholding the local copy stalls it
("no block committed … chain length = 0"); dropping the own-key resolution stalls
it too, with the copy refused as an unregistered sender. 12 core tests; mutations
M1/M3/M4/M5/M6 each kill the tests that name them; workspace 1195 → 1208/0/37.

The exit test itself was lying. `traffic.sh` asserted the chain GREW BY N, and a
20-tx run reported "grew by 0 of 20 — NOT DELIVERED" while COMMIT-OK kept firing
and the tip kept moving: a token's chain is a 5-block sliding window
(`security_level` doubles as max length, `Token::commit_block` trims as it
appends). Fixed in `8b0c19a`: the reader prints `blocks=/sequence=/tip=`, the gate
reads `sequence` (bumped per commit, never on a trim), negative control = stop
`pmesh-committer-1` → "0 of 3 submissions reached a committed block", exit 1.
Named, not fixed: `sequence` counts appends, so a replacement moves it — per-tx
observability needs a window larger than the batch; and data services re-apply
genesis every boot, so chains reset on each `up.sh`.

Exactly-once held under live load: no transaction was committed twice with the
SAME block hash on any node. What DID appear is a separate, larger thing —
each host's Finalizer optimistic-finalizes its own block, its Committer accepts
only that copy and refuses the peers', and a peer fork can replace an
already-committed tip. Four hosts, four chains: written up as
`fact-composite-per-host-fork-divergence`, and now Phase 2's open half. Also named
as a residual: the sentinel's cross-sentinel `Clear` now reaches its own host and
is refused (`unknown action: "Clear"`) — documented at `SENTINEL_ACTIONS`,
deliberately unfixed, because its handler clears the role-SHARED pending registry.
