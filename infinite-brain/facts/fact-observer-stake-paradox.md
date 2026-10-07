---
id: fact-observer-stake-paradox
title: "Paying for observation in stake: the gate that makes monitors cost fault tolerance"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/05/2026: registration is stake-gated (`registration.rs:434`) and the same stake pool feeds leader selection and the quorum denominator (`epoch_manager.rs:77` → `quoruming.rs:55`). So a node that registers to *observe* the network either holds no stake and sees nothing, or holds stake and permanently inflates the denominator while never voting. Worked example: 4 validators × 1000 @67% need 2680/4000; add a 1000-stake observer and they need 3350/5000 but can only ever reach 4000 — 80%, one fault-tolerance slot spent on a monitoring tool. Anything that watches this protocol should avoid joining it."
auto_inject: true
applicable_when: "Designing any monitoring, discovery, diagnostics, bootstrapping, or observer component; or reasoning about what stake means for non-validating nodes"
confidence: 1.0
verified_at: "10/05/2026"
verified_by: "dsh-agent"
staleness_signal: "PARTLY RESOLVED 10/05/2026 — the directory query no longer requires registration. Still live for anything else that registers (see the Resolution section): re-check if the stake gate becomes role-specific, if the quorum denominator moves from the stake snapshot to the live registered set, or if a non-voting observer role is introduced"
tags: [fact, staking, quorum, observability, protocol-design, monitoring, consensus]
edges:
  - target: fact-control-plane-peering
    type: depends_on
    weight: 0.9
    note: "The stake gate sits on the registration path every observer would have to take"
  - target: fact-config-trust-relaxations
    type: supports
    weight: 0.85
    note: "The receiving-half fix: an observer posture that a participant cannot switch on"
  - target: fact-static-binding-replay
    type: depends_on
    weight: 0.85
    note: "Removing registration from the query path only helps if the query itself cannot be replayed; that is a separate property"
  - target: fact-mesh-verification-probe
    type: supports
    weight: 0.9
    note: "This is why mesh health is self-reported by nodes instead of collected by one staked observer"
  - target: task-multihost-testnet-rollout
    type: supports
    weight: 0.8
    note: "Phase 4 (admission, key custody) and Phase 5 (operability) both inherit this constraint"
related: ["[[Mesh verification must be asked of nodes; logs cannot answer it]]"]
source_url: "Empty"
---

# Paying for observation in stake: the gate that makes monitors cost fault tolerance

## The mechanism

Three facts that are each reasonable alone and produce a trap together:

1. **Registration is stake-gated.** `registration.rs:434` runs
   `if !(self.stake_check)(requester_key, &node_type)`; failure returns a
   RegisterAck of `"insufficient stake"`. To be answered by a peer — including a
   directory query — a node must be registered, so it must hold stake for the role
   it asked for.
2. **The stake pool is the validator set.** `StakeStore::to_stake_set()`
   (`epoch_manager.rs:77`) collects *every* key with stake > 0 and that set drives
   leader selection.
3. **Quorum divides by that same total.** `quoruming.rs:55-71` reads
   `total_stake()` from the cached `StakeSet` and compares voting stake against
   `quorum_percentage × total`.

So a positive-stake key is *in the denominator* whether or not it ever signs
anything.

## The arithmetic

| Cluster | Total stake | 67% quorum | Max attainable (observer never votes) |
|---|---|---|---|
| 4 validators × 1000 | 4000 | 2680 | 4000 — 100% |
| + 1 observer × 1000 | 5000 | 3350 | 4000 — **80%** |

The chain still finalizes. What quietly disappeared is the ability to lose a
validator: with 4 honest of 5, one more failure drops to 3000/5000 = 60% and the
epoch cannot finalize. **A monitoring tool was paid for out of fault tolerance.**
Worse, the observer is also selectable as a leader and will not do the work.

## The design rule

> Observation should not require participation. Anything that watches this
> protocol should avoid joining it.

Mesh health now follows it: each node signs a self-report of its own directories
and the pipeline ships the file (`fact-mesh-verification-probe`). No registration,
no links, no stake — the gate never applies, so none of this arithmetic bites.

The unresolved question worth settling before more tooling is built on the control
plane: **should the directory query require stake at all?** A query needs what it
actually verifies — a binding signature and a liveness check — not a stake
position. Gating it on stake is what forces every future observer, discovery
service, and diagnostic into the validator set. (Not traced: whether epoch
reconciliation later evicts or zeroes an observer's stake. If it does, the
denominator self-heals on an epoch boundary and the failure becomes intermittent —
which is harder to diagnose, not easier.)

## Where this generalizes

Anywhere a chain conflates "has standing to speak" with "is a validator": bootstrap
helpers that need to enumerate peers, external explorers that query the network,
watchtowers, bridge relays' monitoring halves, synthetic load generators for load
tests. Each of those either pays stake (and changes the security parameters it is
measuring) or is locked out — and a load generator that inflates the quorum
denominator is a self-inflicted halt, which is how test infrastructure takes down a
testnet.

## Resolution (same day): the gate was membership, not stake

Chasing "why would a query need stake?" through the code produced a correction to
this node's own framing. **The directory query never checked stake.**
`build_directory_response` checked two things: a valid binding signature, and that
the requester was in *our* registry. Stake entered one hop earlier, at
`handle_register`. So the paradox was real in effect — an observer could only
become answerable by registering, and registering needed stake — but the mechanism
was inherited, not designed, and that made it removable without touching validator
admission at all.

What the query gate was actually for, and what survives:

- **Enumeration** of the live validator set to any UDP peer. Legitimate; kept.
- **A reply loop** between peers. Fixed properly by making responses data-only.
- **Cost.** Real and large: a hybrid signature is 3796 B (`crypto.rs:74-75`), so a
  40-entry directory is ~156 KB plus an ML-DSA signature to produce, and every one
  of those bytes rides the Resource-transfer path because the direct-packet cap is
  481 B (`wrapper.rs:79`). A ~200-byte request buys all of it.

The fix that keeps all three: a directory query now signs `query_payload` —
requester rhash, **responder rhash**, **one-shot nonce**, requested type, declared
roles — and an unregistered requester is answered only if the query names us, the
nonce is unseen within `QUERY_NONCE_WINDOW`, and we already have a route to it.
Registration is no longer required to ask who the validators are; contact is.
Validator admission and the transaction-action floor are untouched — note they
share one constant (`global_min_stake`, `config.rs:437` and `action_router.rs:183`),
so lowering it to admit observers would also drop the floor for actions. That is
why "make the per-type minimum zero" does not work either: the global floor still
applies to every type.

**The other half, same day.** `handle_directory_response` refused a response whose
responder was not in *our* registry, so an observer got answered and then discarded
the answer. Making that symmetric is unsafe by default: a validator accepting
directory entries from arbitrary reachable peers installs attacker-chosen keys into
its role buckets, and `send_to_all` fans real pipeline traffic to whatever a bucket
holds. Entries cannot be *misattributed* to real validators — each is bound by its
own key — so the exposure was fake peers and poisoned directories, not impersonation.

The fix that keeps the asymmetry honest is `directory_observer` in config, refused to
any node declaring a consensus role, with the roles read from what the node actually
runs. So an observer consumes answers; a participant cannot turn the protection off.
See `fact-config-trust-relaxations` for why the posture is derived rather than
asserted, and for the test that passed for the wrong reason until a mutation run
exposed it.

**Net effect:** a node with no stake can now learn the validator set — query it,
receive it, and install it — without appearing in the quorum denominator. What
observation costs is now a route and a signature.
