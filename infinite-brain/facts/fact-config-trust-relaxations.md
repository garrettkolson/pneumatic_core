---
id: fact-config-trust-relaxations
title: "A config key that relaxes trust must be unreachable to the node it endangers"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/07/2026: `directory_observer` lets a node apply directory responses from peers it never registered with — necessary for monitors, which hold no stake and so cannot register. It is refused to any node declaring a consensus role, checked against the roles actually declared to the registry rather than what `config.json` claims: a knob a validator could flip to make itself trusting is not a safety property. The attack it prevents is specific — installed attacker keys sit in role buckets, and `send_to_all` fans real pipeline traffic to whatever a bucket holds. Also recorded: the first version of the test for this passed for the wrong reason (reachability refused it), and only a mutation run revealed that the guard it was named for was untested."
auto_inject: true
applicable_when: "Adding any config key, feature flag, or env var that weakens a trust, admission, or verification check; or reviewing whether a test actually isolates the guard it claims to cover"
confidence: 1.0
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If the observer posture is replaced by a first-class non-voting role, if declared roles stop reflecting installed roles, or if directory entries stop being a source of fan-out targets"
tags: [fact, security, configuration, trust, control-plane, testing, mutation-testing]
edges:
  - target: fact-observer-stake-paradox
    type: depends_on
    weight: 0.9
    note: "The observer posture exists because observation used to require joining — this is the receiving-half fix for that"
  - target: fact-static-binding-replay
    type: relates_to
    weight: 0.8
    note: "The sending half of the same path; both halves needed a floor that is not stake"
  - target: fact-control-plane-peering
    type: relates_to
    weight: 0.75
    note: "declared_roles is what the guardrail reads, and the binaries populate it from what they install"
related: ["[[Paying for observation in stake: the gate that makes monitors cost fault tolerance]]"]
source_url: "Empty"
---

# A config key that relaxes trust must be unreachable to the node it endangers

## The rule

When a config key exists to weaken a check, ask: **which node is harmed if the wrong
node sets it?** If the answer is "the one that sets it", the key needs a second
condition that the endangering configuration cannot satisfy — ideally derived from
something the node cannot merely *claim*.

`directory_observer` is the case. A monitor, explorer, or indexer holds no stake, so
it can never register, so the rule "the responder must be a node we registered" made
it discard every directory answer it was given. Opting in fixes that. It would also —
if that were the only condition — let a validator turn off its own protection.

The specific harm is worth stating because it is not the obvious one. A validator
accepting directory entries from arbitrary reachable peers installs **attacker-chosen
keys into its role buckets**. Entries cannot be *misattributed* to real validators
(each is bound by that validator's own key), so this is not impersonation. The harm is
downstream: `send_to_all` fans real pipeline traffic to whatever a bucket holds, so
fake bucket entries are a data-plane delivery oracle, obtained without registering and
without staking a coin.

So the guard is:

```rust
if !self.config.directory_observer { return false; }
if self.declared_roles().iter().any(NodeRegistryType::is_consensus_role) {
    eprintln!("... ignoring the observer relaxation");
    return false;
}
self.peer_is_reachable(&response.responder_rhash)
```

The middle check reads `declared_roles` — which the binaries set from the roles they
actually installed (`node-server/src/node_server/build.rs:232` passes the installed
roles; `committer/src/main.rs:206` passes `Committer`), not from what the config
declares it would like to be. **The posture is derived from behavior, not asserted by
configuration.** A config key can express intent; only observed behavior can be
trusted as a constraint.

The other two conditions each do one job: the opt-in is the operator's decision that
this node watches; reachability is a spam floor — not authority, since the envelope
and per-entry signatures remain the authority and are still verified.

## The test that passed for the wrong reason

The first version of `a_participant_still_refuses_an_unregistered_directory_responder`
built a plain registry and asserted the response was refused. It passed. It also passed
after the `directory_observer` check was deleted entirely — because the fixture has no
transport and no seeded route, so **reachability refused it** and the guard the test
was named for was never exercised.

Fixed by seeding the route, which makes the opt-in the only thing standing between the
participant and the answer. The test now fails when the check is removed.

The general shape to watch for: **a refusal test is only evidence if exactly one
condition in the chain can produce the refusal.** When several guards could each have
said no, assert the one you mean by making the others demonstrably satisfied. Any test
whose assertion is "this was rejected" is a candidate for this bug; the mutation run
finds them, reasoning about them does not — I wrote that test believing it pinned the
opt-in, and it did not.
