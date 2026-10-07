---
id: log-implement-code-20261007-092124
type: log
operation: implement-code
date: "2026-10-07T09:21:24"
namespace: pneumatic
summary: "Closed the receiving half of the observer path: a new `directory_observer` config key lets a node apply directory responses from peers it never registered with, so a monitor's answers stop being discarded — and it is refused to any node declaring a consensus role, with the roles read from what the node actually runs rather than what config claims. The mutation run earned its keep: the test named for the opt-in check passed even with that check deleted, because reachability was refusing the response for an unrelated reason. Suite 1138 → 1143/37/0"
affected_nodes: ["fact-config-trust-relaxations", "fact-observer-stake-paradox", "fact-static-binding-replay", "fact-control-plane-peering", "task-multihost-testnet-rollout", "repo:src/node/registry/registration.rs", "repo:src/config.rs"]
tags: ["log", "implement-code", "security", "configuration", "trust", "control-plane", "testing", "mutation-testing"]
---

Session spans two days; this is the second half of the directory-admission change.
On 10/05 the query path was opened (queries bind responder + nonce, so contact
replaced membership). What remained was that an observer still could not *use* what
it got: `handle_directory_response` refuses a response whose responder is not in our
registry, so a monitor was answered by peers and then discarded every answer.

Made that a decision rather than a guess, because the symmetric fix is unsafe by
default. A validator accepting directory entries from arbitrary reachable peers
installs **attacker-chosen keys into its role buckets**, and `send_to_all` fans real
pipeline traffic to whatever a bucket holds. Entries can't be misattributed to real
validators — each is bound by its own key — so the harm isn't impersonation; it's a
data-plane delivery oracle obtained without registering and without staking a coin.

The shape the user picked: `directory_observer` in config, refused to participants.
Worth keeping the reason it's not just "check the flag":

```rust
if !self.config.directory_observer { return false; }
if self.declared_roles().iter().any(NodeRegistryType::is_consensus_role) { return false; }
self.peer_is_reachable(&response.responder_rhash)
```

`declared_roles` is seeded from installed roles — `build.rs:232` passes what the
composite actually installed, `main.rs:206` passes `Committer` — so the posture is
**derived from behavior, not asserted by configuration**. A config key can state
intent; only observed behavior can serve as a constraint. Three conditions, three
jobs: opt-in is the operator's decision, the role check is what a participant cannot
step around, reachability is a spam floor and *not* authority (envelope and per-entry
signatures remain the authority and are still verified).

**The mutation run found a fake test.** `a_participant_still_refuses_an_unregistered_
directory_responder` passed with the `directory_observer` check deleted outright: the
fixture has no transport and no seeded route, so *reachability* was refusing the
response and the guard the test was named for was never exercised. Fixed by seeding
the route so the opt-in is the only thing standing between the participant and the
answer — and now deleting the check fails the test.

The general shape, which I'd written off as a solved habit: **a test asserting "this
was rejected" is only evidence if exactly one condition in the chain can produce the
rejection.** Several guards could each have said no here. Reasoning about it didn't
catch it — I wrote that test believing it pinned the opt-in.

Also: adding a field to `Config` again broke exhaustive literals — 19 sites — matching
the 18 predicted by the last time. That cost is now a known quantity rather than a
surprise, which is the only benefit of paying it twice.

Deliberately left alone: an observer's peering loop still sends `Register` and gets
"insufficient stake" back on every tick. Noisy, but a visible failure beats a silent
one, and a peer that cannot answer a question it kept asking is exactly the kind of
thing that would be invisible if I made it stop asking.

Suite **1143 / 37 / 0** (+5). Four guardrails on the new path are mutation-verified:
opt-in, consensus-role refusal, reachability floor, and envelope verification still
running under the relaxation. Files restored byte-identical after each mutation.
