---
id: log-implement-code-20261002-232409
type: log
operation: implement-code
date: "2026-10-02T23:24:09"
namespace: pneumatic
summary: "Multi-host prep for a cloud testnet. Found and fixed the hard blocker: RnsNodeConfigBuilder defaults listen_ip to 127.0.0.1 and node-server never overrode it, so the composite was unreachable across hosts while looking healthy — now a shared Config::rns_listen_ip plus a loadable, fail-closed ip_address. Added per-host placement to testnet-gen (peers dial real addresses, one UDP range per machine) and tests that invoke the binary. Found --validators n silently truncating to a multiple of 4. Suite 1084 → 1101/37/0"
affected_nodes: ["fact-transport-loopback-bind-default", "fact-test-suite-cloud", "fact-testnet-generator", "concept-env-driven-config", "task-testnet-launcher", "repo:testnet-gen/", "repo:src/config.rs", "repo:node-server/src/node_server/build.rs"]
tags: ["log", "implement-code", "transport", "bind", "deployment", "testnet", "cloud", "correction"]
---

Asked what the deployment question actually turns on before writing anything,
because "deploy to a cloud provider" has an obvious shape (Terraform + instances)
that is worthless if the transport can't cross a host boundary. Three greps
answered it faster than any planning.

The blocker was real and quiet: `RnsNodeConfigBuilder::new()` defaults
`listen_ip` to `127.0.0.1`; the committer overrode it with a local helper, and
`node-server` never called `with_listen_ip` at all. So the composite bound
loopback — booted, announced, logged nothing worse than "no live route" — and
could not be reached by any other machine. It survived because every test in the
repo is a single-host mesh, where a loopback bind is indistinguishable from a
correct one. Worse, `Config.ip_address` was never read from config.json at all
(`Config::build` hardcoded `::`), so the committer's configured-address branch
was dead code and there was no way to express a bind choice.

Fixed the way the repetition demanded: `Config::rns_listen_ip()` in core as the
single rule for both binaries (the committer's copy deleted, not left in place),
`ip_address` loadable through `ConfigSpec`, malformed values stopping boot rather
than falling back to "any" — because the fallback reproduces exactly the
invisible-failure shape that caused this. Verified behavior change: the composite
now binds all interfaces, and the full suite is unchanged at 1101/37/0. That
non-result is itself the lesson — binding "any" is a superset of binding
loopback, so this suite cannot detect a loopback-only bind in either direction.

For the generator, the concept that made the design fall out cleanly was
`Placement`: "where the nodes run" decides two things at once — whether port
windows must be disjoint, and which address a peer dials. As one value, the
broken combination (one node per machine, everyone dials 127.0.0.1) stops being
representable. Per-host placement means the address disambiguates, so all
machines share one base port and a security group opens one UDP range per
instance instead of one per node. The two-pass split (identities, then configs)
existed for ordering reasons and turned out to be exactly what provisioning
needs: keys before machines, addresses after.

Two things I want to record as corrections to myself rather than bury:

1. My first "verification" of the two-pass flow was **vacuous**. The generation
   had failed and I'd sent stderr to /dev/null, so `cat` matched nothing and both
   keystore digests were the hash of empty input — "KEYS PRESERVED" was printed
   from nothing. The stderr happened to surface the missing files and I caught it,
   but the check should have asserted the files existed before comparing them.
2. Chasing that failure exposed a genuine bug: `--validators n` computed `n / 4`
   and dropped the remainder, so `--validators 10` silently built 8 nodes and
   `--validators 1..3` failed as "every role count is zero". The help text said
   "n nodes per role", the code comment said the same, and the code did neither.
   Now distributes the remainder in role order and prints the split.

Because flag parsing lives in `main.rs`, unreachable from the library, I added
`tests/cli.rs` to invoke the actual binary — four tests including the exact-count
property that caught the truncation. The address test is mutation-verified:
replacing the peer's address with a shared constant fails
`per_host_placement_writes_each_peers_own_address` and nothing else, which is the
point, since a single-host cluster cannot otherwise distinguish correct output
from "everyone dials loopback".

Suite 1084 → 1101/37/0. What I deliberately did not do: no Terraform (the user
scoped to code fixes, provider-neutral), no relay work, and no claim that density
is solved — leaves still cannot route through leaves, so the mesh needs flat
routable L3, and dense mesh has still never been exercised off loopback. The next
milestone is 4 nodes on 4 real instances proving mesh formation, not 40.
