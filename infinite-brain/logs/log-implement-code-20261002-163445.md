---
id: log-implement-code-20261002-163445
type: log
operation: implement-code
date: "2026-10-02T16:34:45"
namespace: pneumatic
summary: "Built testnet-gen: generates keystores (via NodeIdentity::load_or_create), per-node config.json with both-sided bootstrap_peers, per-node env dirs, Ed25519-keyed genesis and a manifest — closing task-testnet-launcher's keys/peer-port-matrix blocker. Added PNEUMATIC_CONFIG_FILE / PNEUMATIC_ENV_DIR overrides so N nodes share one host. Derived the role fan-out graph from the send_to_all call sites and measured it: a full mesh minus executor↔executor, so topology pruning never lowers the worst node. Found and fixed two of my own bugs (reversed j-rule in the emitter; --topology full-mesh silently rewritten to role-graph). Suite 1065 → 1084/37/0"
affected_nodes: ["fact-testnet-generator", "fact-fanout-graph-density", "fact-test-suite-generator", "concept-env-driven-config", "task-testnet-launcher", "repo:testnet-gen/", "repo:src/config.rs"]
tags: ["log", "implement-code", "testnet", "generator", "config", "topology", "density", "correction"]
---

Continued the testnet work onto the item the last two turns had converged on: a
central generator, because the peer/port matrix is not derivable locally — the
j-rule depends on another node's peer list.

Asked and got two decisions before writing: topology mode behavior (both modes,
role graph above a threshold) and tool shape (a workspace crate, not a script).
The crate choice was worth more than it looked: keystores carry ML-DSA/ML-KEM
keypairs and the config has a serde shape both binaries parse, so writing them in
another language would have created a second definition of a contract this repo
has already been bitten by twice.

Derived the topology from the code rather than from the pipeline tests' doc
comments, and that produced the turn's most useful result. Reading
`send_to_all` call sites: sentinel→{executor,finalizer,sentinel},
executor→{finalizer}, finalizer→{committer,finalizer,sentinel,executor},
committer→{committer,executor,sentinel}. Exactly one of the ten unordered role
pairs has no send in either direction — executor↔executor. So the "role graph"
the user approved is a full mesh minus intra-executor links. Measured with the
generator: 5.8% saving at 10-per-role, 50.5% with 30 executors against 12 others.
But the number that matters for "can this run on one host" is interfaces on the
*worst* node, and pruning does not lower it — at 4S/30E/4F/4C the dense-half
still binds 41 sockets on each non-executor. The honest conclusion is that
density is a relay question or a fewer-nodes question, not a topology question,
and relay is the one thing this repo has never exercised. Recorded as a fact
because it is code-derived ground truth that answers an open task item.

Two bugs, both mine, both caught by artifacts rather than by reading:

1. **The emitter had the j-rule backwards.** It called
   `plan.listen_port_for(peer)` — the port the *peer* forwards to reach *us* — and
   wrote that into our `bootstrap_peers`. The in-memory topology test caught it
   first because it asserted the forward port lands inside the target's range,
   which the reversed direction cannot satisfy. The failure was the correct
   diagnosis, not a test bug, and it is precisely the failure mode the module docs
   describe: every config populated, every node booted, zero links.
2. **`--topology full-mesh` was silently rewritten to role-graph above the
   threshold.** I had attached `auto`'s rule to `FullMesh`. Discovered because the
   40-validator "full mesh" run reported role-graph numbers — I checked the
   manifests instead of trusting the summary line, and the arithmetic disagreed
   with my own analysis. An explicit operator choice being quietly overridden is
   the kind of thing that makes someone debug a graph they never asked for. Fixed
   by making `Auto` a distinct mode with a regression test, and `Mesh::build` now
   rejects `Auto` rather than guessing.

Verification aimed at the real consumers rather than at shapes the tests define:
emitted configs parse as `ConfigSpec`, env dirs build through
`EnvironmentMetadata::load_from_spec`, keystores reload to identical identities,
and `tests/boot_config.rs` drives a generated node through the actual
`Config::build()`, asserting the identity it loads is the one genesis was keyed
by. The j-rule test is mutation-verified — reversing the emitter's direction fails
it with `sentinel-1 must forward to sentinel-2 on base 21009 + j 0`. One assertion
of my own was wrong (full nodes have 5 role buckets including Archiver, not 4);
the test failing on my expectation rather than the code was the useful outcome.

Also added `deploy/generated/` to `.gitignore`: generated trees hold node private
keys, and they were not ignored. Suite 1065 → 1084/37/0. Remaining on the task:
the up/down/status launcher — everything it needs is now in `manifest.json` — and
converting the three older seeded integration tests.
