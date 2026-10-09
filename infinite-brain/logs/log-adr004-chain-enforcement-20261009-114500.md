---
id: log-adr004-chain-enforcement-20261009-114500
type: log
operation: convert-note
date: "2026-10-09T11:45:00"
namespace: pneumatic
summary: "ADR-004 made load-bearing across the whole transaction chain (selection attested on the primary path, single-recipient hops, Finalizer and Committer role gates, decision owned by the transaction); fact-composite-per-host-fork-divergence RESOLVED on the primary path; two new facts (fact-h12-assumes-lockstep-execution — with one producer the commit path requires lockstep execution, and fact-hybrid-signature-not-deterministic — ML-DSA randomizes, so no signature may be compared byte-for-byte); rollout task's Phase 2 blocker moved from routing to convergence; 11 new tests, 11 mutations run, suite 1208 → 1225/0/37"
affected_nodes: ["decision-deterministic-per-tx-routing", "fact-composite-per-host-fork-divergence", "fact-h12-assumes-lockstep-execution", "fact-hybrid-signature-not-deterministic", "task-multihost-testnet-rollout", "decision-sentinel-routing-authority"]
tags: ["log", "adr-004", "routing", "finalizer", "committer", "multihost", "verification"]
---

# ADR-004, applied for real

The user's question was the whole diagnosis: *"there should only be one finalizer per
transaction (decided deterministically), and there is only one transaction per block, so
why would there be a potential for block divergence?"* They were right, and the honest
answer was that the design had never been applied on the primary path. `3c0d867` and
`89584e5` apply it.

## What landed

- **Transport.** `Transaction.selection` carries a `SelectionRecord` — assigned
  Finalizer, epoch, salt, Sentinel attestation — excluded from `CanonicalTransaction` so
  it rides into the block hash without becoming something the sender signed.
- **Selection on the standard path** (`Sentinel::routing_decision_for`), attested, with
  `Sentinel::staked_finalizers` restricting candidates to Finalizers this node can
  address, and an empty candidate set refused with both counts named.
- **One decision per transaction**: a record that arrives is honoured, not recomputed —
  the rule the rehearsal proved necessary.
- **Single-recipient hops** in the executor, failing when the assigned key is
  unreachable, instead of broadcasting and hoping.
- **Role gates** at the Finalizer (preload, vote intake, finalize choke point) and the
  Committer (`commit_authority`, `gossip_authority`, `held_decision`), each verifying
  the attestation **and** that the attester resolves to a registered Sentinel —
  signature-only verification is vacuous against a Finalizer that signs itself.
- Two defects found on the way and fixed: `get_finalizer_key` read a state the executor
  had already moved past (the broadcast hid it), and `send_to_peers_blocking`'s direct
  branch built a tokio runtime on a thread already inside one, which panics outright —
  now `direct_send_blocking` owns a runtime where it can and hands the send to a thread
  that can when it cannot.

## What the rehearsal proved, in three builds

| Build | Measured |
|---|---|
| `phase2n` (before) | 4 txs → 4 hosts each building their own block; committer-1 accepted whichever copy matched its own booking |
| `adr004` (routing + gates) | 1 tx → committer-1 appended **two** blocks with two different hashes; other hosts committed nothing |
| `adr004c` (+ decision owned by the tx, committer compares to what it holds) | 6 txs → exactly **one** host produces and commits; three refuse |

Multiplicity is gone; convergence is not, and the reason is recorded as
`fact-h12-assumes-lockstep-execution`: the commit path compares the block's canonical
hash — which includes `result_hash` — against the receiver's *own* locally executed copy,
so the receiver can only accept a block whose execution it happens to have reproduced.
That was always true and always invisible, because before routing every block was built
from its own producer's execution. The node lays out the three ways out (install the
block's result / re-execute at the pinned parent / gossip results ahead of blocks) and
names `deploy/multihost/tips.sh` as the exit test.

## Two claims this repository was making that were false

1. `Committer::commit_authority`'s own doc said H12 binds the routing decision "for
   free". It does not — `selection` is deliberately outside `CanonicalTransaction`, and
   that is *correct* (a routing record is not user intent); the comment had papered over
   a real hole that the mesh then demonstrated.
2. `src/crypto.rs` said the ML-DSA half of `sign_data` is deterministic. It is not: the
   same key over the same bytes twice gives two different 3,796-byte signatures. Recorded
   as `fact-hybrid-signature-not-deterministic`, with the consequence that no code may
   compare signatures byte-for-byte — which is why record comparison goes through
   `SelectionRecord::same_decision_as`.

## Verification

Suite 1208 → **1225 passed / 0 failed / 37 ignored**, 11 new tests. Eleven mutations were
run against the new behaviour — candidate filter, the three Finalizer gates, the vote
replay, the committer's routing and held-decision checks, the engine-boundary strip, the
single-recipient hop, the nested-runtime fix, the honour-carried-decision branch — and
every one fails its named test. Two mutations initially "survived" because the test
filter did not include the right test (`a_composite` instead of `pipeline_wasm_sum`, and
the blocking `send_to_peers` instead of the async one); both were re-run against the
correct site and caught, which is worth recording as the more common way a mutation check
lies.

## Residuals, named not fixed

Persisted role-scoped `FinalizerSet` (Phase 7's responsible-set work) so two Sentinels
cannot hold different candidate views; re-derivation verification of the record (needs an
epoch-keyed snapshot read at the committer); a Byzantine Sentinel signing two decisions
for one transaction (attributable, not prevented — ADR-007's boundary); shielded-path
enforcement (explicit exemption while the shielded transaction is an S3.1 placeholder).
Harness change noted for honesty: `up.sh` now publishes every data-service sidecar,
because while only one of four chains could be read, the divergence had no observable
signature.
