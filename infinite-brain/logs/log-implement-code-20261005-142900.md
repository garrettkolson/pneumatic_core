---
id: log-implement-code-20261005-142900
type: log
operation: implement-code
date: "2026-10-05T14:29:00"
namespace: pneumatic
summary: "Directory queries no longer require registration, so an observer can learn the validator set without stake. Chasing the user's question 'is there a reason a directory query would require stake?' found that the query never checked stake — it checked *membership*, with stake inherited one hop earlier at Register — and that the query's credential was a static binding with no nonce, responder, or timestamp, making one observed query a permanent replayable subscription. Queries now bind responder + one-shot nonce; unregistered askers are answered only if the query names us, the nonce is fresh, and we already have a route. Suite 1132 → 1138/37/0"
affected_nodes: ["fact-observer-stake-paradox", "fact-static-binding-replay", "fact-control-plane-peering", "fact-mesh-verification-probe", "task-multihost-testnet-rollout", "repo:src/node/registry/registration.rs", "repo:src/rns/identity.rs"]
tags: ["log", "implement-code", "control-plane", "security", "replay", "admission", "staking", "protocol-design"]
---

The question was "is there a reason a directory query would require stake?" Answer:
three reasons to gate it, none of which stake serves — and asking it corrected
something I had written down with confidence.

**The query never checked stake.** `build_directory_response` checked a binding
signature and that the requester was in our registry. Stake entered at
`handle_register`, one hop earlier. The paradox I had recorded was real in *effect*
(an observer could only become answerable by registering) but the mechanism was
inherited, not designed — which is why it came out without touching validator
admission.

Three reasons the gate exists, kept separate because they need different fixes:
**enumeration** of the live validator set (legitimate, kept), a **reply loop**
(already fixed properly by data-only responses), and **cost** — a hybrid signature
is 3796 B, so a 40-entry directory is ~156 KB plus an ML-DSA signature, and with a
481 B direct-packet cap every byte rides Resource transfer. ~200-byte request buys
that.

Then the finding that actually decided the design: **`binding_payload` signs only
`(rhash, requested_type, requester_types)`.** No nonce, no timestamp, no
counterparty. So the gate that looked like access control metered nothing — one
observation of a legitimate query was a permanent subscription, replayable against
every peer that key is known to. Opening the path without fixing that would have
been a regression dressed as a feature.

What landed: `query_payload` (five-tuple, including responder and one-shot nonce),
an admission rule of *target is us → signature valid → we can reach them → nonce
unseen → answer*, and `Register`'s stake gate untouched. Registration is no longer
required to ask who the validators are; **contact is**.

Also worth keeping: `global_min_stake` is one constant serving both registration and
transaction actions (`config.rs:437`, `action_router.rs:183`), so "just set the
per-type minimum to zero" does not work — the global floor applies to every type —
and lowering it to admit observers would also drop the floor for actions. Shared
knobs make cheap-looking config changes expensive.

Three judgement calls:

- **Domain separation, tested both directions.** A new signature type next to an old
  one creates a smuggling problem. A query signature signs a different tuple shape,
  so it cannot register anyone — and I asserted that in a test rather than assuming
  it, because the attacker in it holds no stake. The weak-to-strong direction is the
  one that would have quietly reintroduced the gate I was removing.
- **Burn the nonce last.** A query refused for a transient reason (route not up yet)
  must not consume its own nonce. With the check earlier, "not yet" becomes "that
  query is permanently dead" — the class of bug that reads as an unexplained stall.
- **Tests may not be laxer than production.** The first cut treated `network = None`
  as "cannot check → allow", which would have made the fast suite permissive in a way
  no real node is — a suite that passes for reasons production would not, which is
  worse than no test. Fail closed; seed reachability explicitly in tests; leave the
  announce path to the live-loopback tests.

All three new guards are load-bearing: disabling the replay check, the reachability
check, and the target check each failed its test. Files restored byte-identical.

**The other half is not done, and it is a decision, not a task.**
`handle_directory_response` refuses a response whose responder is not in *our*
registry, so an observer gets answered and then discards the answer. Making that
symmetric is not safe by default: a validator accepting entries from arbitrary
reachable peers installs attacker-chosen keys into its role buckets, and `send_to_all`
fans real pipeline traffic to whatever is in a bucket. Entries can't be misattributed
to real validators — each is bound by its own key — so the exposure is fake peers and
poisoned directories, not impersonation. The narrow shape is a posture that
distinguishes observers from validators, which is the non-voting observer role.

Suite **1138 / 37 / 0** (+6). One wire caveat for whoever deploys: rmp encodes structs
positionally, so `NodeRequest` frames are not byte-compatible across this change even
though old queries are still honored semantically. Everything on a network moves
together.
