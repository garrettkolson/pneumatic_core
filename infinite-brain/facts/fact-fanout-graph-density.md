---
id: fact-fanout-graph-density
title: "The role fan-out graph is a full mesh minus executor↔executor"
type: fact
namespace: pneumatic
visibility: namespace
summary: "10/02/2026, read off the send_to_all call sites: sentinel→{executor,finalizer,sentinel}, executor→{finalizer}, finalizer→{committer,finalizer,sentinel,executor}, committer→{committer,executor,sentinel}. Only the executor↔executor pair class is absent, so pruning to the protocol graph saves 5.8% of interfaces at equal role counts, ~50% when executors dominate, and nothing at all on the worst node."
auto_inject: true
applicable_when: "Sizing a testnet, choosing a topology, or asking how many validators fit on one host"
confidence: 1.0
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "Any change to the send_to_all call sites listed below (sentinel/src/transaction_notifier.rs, executor/src/executor.rs, finalizer/src/message_dispatcher.rs, committer/src/**) or the introduction of sharded subsets that would not require every peer of a role"
tags: [fact, topology, fanout, density, transport, testnet, capacity]
edges:
  - target: concept-rns-transport
    type: depends_on
    weight: 0.9
    note: "Interfaces are point-to-point and leaves cannot route through other leaves, so a required send implies a required link"
  - target: fact-testnet-generator
    type: supports
    weight: 0.9
    note: "The generator's role-graph mode is this graph, and its cost report is what measured these numbers"
  - target: task-testnet-launcher
    type: supports
    weight: 0.85
    note: "Answers the open density item: topology pruning does not lower the binding constraint"
  - target: concept-node-registry
    type: derived_from
    weight: 0.7
    note: "send_to_all fans out to every entry of a role's directory, so 'knows all of role X' is the current contract"
related: ["[[RNS transport: the production inter-node wire (RnsNetwork over rns-net, Resource transfer for large payloads)]]"]
source_url: "Empty"
---

# The role fan-out graph is a full mesh minus executor↔executor

Derived from the code, then measured with the generator, 10/02/2026. `send_to_all`
fans out to **every** entry of a role's directory, and RNS links are
point-to-point (leaves cannot route through other leaves), so *a required send
implies a required link*. Reading the call sites gives the whole graph:

| Sender | Targets | Evidence |
|---|---|---|
| Sentinel | Executor, Finalizer, Sentinel | `sentinel/src/transaction_notifier.rs:42,105,137,152,167,186` |
| Executor | Finalizer | `executor/src/executor.rs:867,904` |
| Finalizer | Committer, Finalizer, Sentinel, Executor | `finalizer/src/message_dispatcher.rs:71,105,133,166-175,203-209,239-245` |
| Committer | Committer, Executor, Sentinel | `committer/src/committer/quoruming.rs:136-139,164-167`, `block_services.rs:135,167` |

Of the ten unordered role pairs, **exactly one** has no send in either direction:
**executor↔executor**. Every other pair is required, and notably the committer
never sends to `Finalizer` — yet `Finalizer → Committer` still forces the link.

Measured with `pneumatic_testnet_gen` (interfaces per node = UDP sockets bound +
threads spent on them):

| Cluster | Full mesh | Role graph | Worst node (role graph) |
|---|---|---|---|
| 40 validators, 10/role | 1560 (39 each) | 1470 (**−5.8%**, executors 30) | **41 → still 39** |
| 42 nodes, 4 sent / 30 exec / 4 fin / 4 comm | 1722 (41 each) | 852 (**−50.5%**, executors 12) | 41 → **still 41** |

**The conclusion that matters for planning.** The role graph's benefit scales with
*executor share*, not cluster size — executors are the only role that can be
pruned, and they are cheap to run but expensive for everyone else, because every
sentinel, finalizer and committer must link to all of them. **The binding
constraint for one host is interfaces on the worst node, and topology pruning does
not lower it.** At 4 S + 30 E + 4 F + 4 C, the half-dense graph still puts 41
sockets on each of the 12 non-executor nodes.

So the levers that actually reduce the worst node are: fewer total nodes per role,
or relay (`transport_enabled: true`), which this repo has **never exercised** —
the relay path is the untested variable, not a known-working one.

Second-order note: this graph is a consequence of "knows every node of role X"
being the current contract. If epochs ever assign a finalizer a *subset* of
committers, the graph thins by itself and this fact's numbers change.
