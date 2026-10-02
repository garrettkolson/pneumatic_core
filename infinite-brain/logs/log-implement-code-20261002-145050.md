---
id: log-implement-code-20261002-145050
type: log
operation: implement-code
date: "2026-10-02T14:50:50"
namespace: pneumatic
summary: "Built the control-plane peering initiator (src/node/registry/peering.rs: Register/directory-Request/Heartbeat senders + a 10 s liveness loop, declared_roles as the single self-description) and wired it into both binaries; fixed five defects it exposed — RegisterAck filing the responder under the requester's role, a bare NodeRequest that decoded as an empty NetworkPacket, ~5.9 KB replies sent on the 481 B direct-packet path, a directory response that echoed a Request (unbounded peer-to-peer reply loop), and a directory answered to any caller; composite now honors rns_port and handles directory responses; suite 1047 → 1064/37/0 including the first two-node-over-UDP peering end-to-end test"
affected_nodes: ["fact-control-plane-peering", "fact-control-plane-silent-drop-paths", "fact-register-ack-bucket-placement-defect", "fact-test-suite-peering", "concept-node-registry", "task-testnet-launcher", "repo:src/node/registry/peering.rs", "repo:tests/peering_e2e.rs"]
tags: ["log", "implement-code", "peering", "control-plane", "node-registry", "rns", "defect-fix", "testnet"]
---

Continuation of the testnet work: the data service had removed the universal boot
gate, and the user chose to proceed with the peering initiator — the piece that
stands between "nodes boot" and "nodes can talk".

Orientation was vault-first (INDEX → concept-node-registry, concept-rns-transport,
task-testnet-launcher), then code. The task node's claim that no production code
sends a control message was re-verified rather than trusted: `register_peer` had
test-only callers and `handle_register_ack` had never been reached from a binary.
That claim turned out to be the beginning of a larger story.

Design first, in the open, because two decisions drove everything else. (1)
Builders separated from senders: a `Register`'s interesting properties are its
signature and its role set, both decided before any socket exists, so
`build_*`/`send_*` splits let 16 tests assert them without a transport. (2) The
registry needed to know which roles *this node* serves: a receiver admits a peer
under every type it declares, and `config.node_registry_types` is all four for
any full node — so sending the config verbatim would file a committer-only binary
in every peer's Finalizer/Executor/Sentinel bucket. Hence `declared_roles`,
seeded from config, narrowed by the composite to its installed plugins, read per
loop round so an epoch's role change is advertised without restarting.

Then the findings, in the order they surfaced. A test I expected to pass failed
with "Register control frame is 5921 B, over the 481 B direct-packet cap":
`sign_binding` is the hybrid `[Ed25519 · ML-DSA pk · ML-DSA sig]`, so every
control frame is ~5.9 KB and the direct path can never carry one. That reframed
the whole boot story — peering cannot happen at t=0, only after a peer's announce
makes its route live — so `send_control` reports "no live route yet" instead of
blocking 30 s per peer inside the loop. Grepping for the transport sends in the
registration handlers then showed `handle_register_ack` filed the responder under
the ack's `node_type`, which names the *requester's* type: a finalizer filing its
committer peer into its own Finalizer bucket, invisible in tests because every
existing test used the same role on both sides. And the directory response carried
a `Request` echo in `control` while both binaries dispatch `control` and `data`
independently — two peers would answer each other's answers forever, with no stop
condition, and the handler verified nothing, so the echo was answerable and any
UDP peer could enumerate the validator set.

None of these five were reachable by inspection of the receiving code alone. The
receiving code was, as the vault recorded it, "sound" with line-number citations.
What was missing was a sender: three of the five only become observable once a
packet crosses a socket. That is recorded as the transferable lesson in the
silent-drop-paths fact.

Verification went further than the unit level because the bugs found were
transport-level. `tests/peering_e2e.rs` starts two real `RnsNetwork` nodes on UDP
with deliberately asymmetric roles — so the ack bug cannot pass — waits for both
routes to go live, runs the peering loop, and asserts each node lands in the
other's correct bucket. It converges in 0.8 s, stable across four runs. Full
workspace suite 1047 → 1064/37/0, ignored count unchanged at 37.

Vault: three new facts plus the baseline, `concept-node-registry` extended with
the sender half, `task-testnet-launcher` downgraded from OPEN to PARTIAL with the
peering blocker marked done. While syncing `_system/INDEX.md` I found its section
headers and totals drifting from files on disk (fact, question and event headers
each under-counting; totals 123 vs 128 actual) — rows themselves were complete,
so this was header rot rather than missing nodes; reconciled and noted in the
index header. Two launch blockers remain for the 20-40 node target: pre-generated
keystores feeding a centrally computed peer/port matrix (the j-rule is not
derivable locally by one process), and the density question — control frames now
known to require live routes on every edge makes full-mesh density a transport
question, not a launcher question.
