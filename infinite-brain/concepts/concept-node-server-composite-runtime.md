---
id: concept-node-server-composite-runtime
title: "node-server composite runtime — one process, four role-plugins"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Composite host: RoleSelector picks role-plugins from own stake against dual floors, RoleDispatcher routes actions to the owning role fail-closed, epoch coordinator fans advances; shared DI bundle."
auto_inject: false
applicable_when: "Working on node-server/, role installation/selection, in-process dispatch, or full-node deployment topology"
confidence: 1.0
verified_at: "09/23/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when node-server/src/node_server.rs or the node_server/ module dir, role_selector.rs, or role_dispatcher.rs change their layers or the plugin set"
tags: [node-server, composite, role-plugin, runtime, in-process-dispatch]
edges:
  - target: event-node-server-composite
    type: derived_from
    weight: 0.9
    note: "The composite runtime is the deliverable of that commit"
  - target: fact-workspace-layout
    type: related_to
    weight: 0.8
    note: "node-server sits at the top of the dependency tree — depends on all four role crates, none depend on it (Cargo.toml comment)"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.9
    note: "UnknownAction/AmbiguousAction rejected; missing environment is a hard error; stake miss reports 0 and installs nothing"
  - target: concept-executor-role
    type: related_to
    weight: 0.8
    note: "Hosts the executor as a plugin (max_in_flight=100 in build_role_plugin); same for sentinel, finalizer, committer"
  - target: pillar-block-lattice
    type: related_to
    weight: 0.7
    note: "Composite mode lets one process play all four pipeline roles for full-node deployments"
related: []
source_url: "Empty"
---

# node-server composite runtime — one process, four role-plugins

The node-server crate is "the runtime" — a single process that hosts Committer, Sentinel, Executor, and Finalizer as role-plugins, with RNS as the only external wire (`node-server/src/lib.rs:1-12`). It is deliberately built on a fresh in-process backbone: not the dead `pneumatic_core::server::ThreadPool` (a gate test `no_threadpool_dependency` scans every `src/**/*.rs` for references — `lib.rs:76-84`) and not RNS.

Three layers (all code-verified):

- **RoleSelector** (Phase 1, `role_selector.rs`): "role-selection-by-stake" — the headline behavior. Reuses `meets_minimum_stake` (the same AND-of-two-floors primitive the registration gate enforces) so "how much stake does a key need" has one source of truth. `select()` reads own stake once per call (miss ⇒ 0 ⇒ installs nothing, fail-closed) and filters `NodeRegistryType::iter()` against global + per-type floors; `select_primary` resolves ties by `Finalizer > Executor > Sentinel > Committer`, matching `NodeRegistry::select_registration_node_type`.
- **RoleDispatcher** (Phase 2/4, `role_dispatcher.rs`): thin in-process router over the inbound bus — inspects only the `action` string, never the body, and forwards to the single installed role whose `allowed_actions()` claims it. Fail-closed: `UnknownAction` and `AmbiguousAction` (two roles claiming one action = wiring bug). Role plugins implement `RoleHandler` (role + actions + `handle` returning a `Send`-boxed future so `Box<dyn RoleHandler>` can cross `.await`/spawn) and `RoleHost` (epoch advance + shutdown lifecycle) — one boxed handle serves both.
- **NodeServer / epoch coordinator** (Phase 5; struct in `node_server.rs`, lifecycle fns in `node_server/transport.rs`, epoch fns in `node_server/epoch_coord.rs` after the 09/23 split): owns the shared DI bundle (one build, shared across N plugins — generalizing the committer `main.rs` boot recipe), polls `EpochBoundaryDetector`, fans epoch advances and shutdown to the installed plugins; `roll_forward`/`recompute_role_set` let the role set change per epoch.

`build_runtime` (`node_server/build.rs` after the 09/23 split; re-exported at `node_server::build_runtime` so the pre-split path survives) tolerates boot failures (transport down, data service unreachable, stake-index warm-up) and boots anyway; only a missing environment is a hard error. `build_role_plugin` (`node_server/plugins.rs`; kept WHOLE — the single construction site of the shared `Arc<ShieldedPool>` is the S5.4 invariant) constructs each plugin from the shared bundle — `Archiver` has no plugin (`None`). Full 09/23 layout: `node_server/{build, plugins, epoch_coord, transport, role_adapters}.rs` + `tests/{helpers, build, transport, epoch, shielded, e2e}.rs`; tests share fixtures via `tests/helpers.rs` (the committer convention).
