---
id: fact-data-user-trait-default-defect
title: "DefaultDataProvider ignored its configured source for user lookups"
type: fact
namespace: pneumatic
visibility: namespace
summary: "RESOLVED 10/02/2026: get_user/save_user were private inherent methods on DefaultDataProvider, so the trait impl inherited the trait default that builds a FRESH provider with the DEFAULT local source — every Arc<dyn DataProvider> caller silently ignored PNEUMATIC_DATA_ADDR/PNEUMATIC_DATA_SECRET for user reads."
auto_inject: true
applicable_when: "Debugging a node that boots but installs no roles or has registrations rejected, or any change to the DataProvider trait's default method bodies"
confidence: 0.4
verified_at: "10/02/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when `impl DataProvider for DefaultDataProvider` implements get_user/save_user (it does since 10/02/2026) — but re-read if the trait defaults are ever reintroduced for any method"
tags: [fact, data-provider, trait-defaults, defect, resolved, rust-visibility, testnet]
edges:
  - target: concept-data-provider
    type: related_to
    weight: 0.95
    note: "The defect is inside the DataProvider trait's default-method design"
  - target: concept-node-server-composite-runtime
    type: related_to
    weight: 0.8
    note: "Composite role selection reads User.stake — the read this defect misdirected"
  - target: concept-node-registry
    type: related_to
    weight: 0.8
    note: "The registration stake gate (StakeIndex) reads get_user; a misdirected read means stake 0 and every registration rejected"
  - target: fact-data-service
    type: related_to
    weight: 0.7
    note: "Found the day the data service landed, by driving a real client over a real socket instead of an in-memory stub"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.5
    note: "The failure was silent rather than fail-closed — the provider answered from a different endpoint instead of erroring"
related: ["[[DataProvider: MsgPack data-store trait with UDS-first local transport and HMAC option]]"]
source_url: "src/data.rs:247 (trait impl) / src/data.rs:39-45 (the trap defaults)"
---

# DefaultDataProvider ignored its configured source for user lookups

Code- and runtime-verified 10/02/2026; fixed same day.

**The shape.** `impl DefaultDataProvider` (an *inherent* block) held private
`fn get_user` / `fn save_user`, while `impl DataProvider for DefaultDataProvider`
implemented eleven other methods and **not** those two. So the trait impl
inherited the trait's default bodies:

```rust
fn get_user(&self, key: &Vec<u8>, partition_id: &str) -> Result<User, DataError> {
    DefaultDataProvider::new().get_user(key, partition_id)   // fresh provider, default source
}
```

`DefaultDataProvider::new()` carries `default_source()` — the per-UID UDS path —
and a secret-less factory. So through any trait object, `with_source()` and
`with_secret()` were **discarded for user reads**.

**Why 1029 tests missed it.** Method resolution picks the inherent method when it
is *visible*; from inside `src/data.rs` (its own wire tests) it is, so those
tests exercised the correct path. Outside the module the private inherent method
is invisible, so both concrete and `Arc<dyn DataProvider>` calls resolved to the
broken default — and every production caller (both node binaries, `ActionRouter`,
`StakeIndex`, `DataStakeProvider`) is outside the module.

**Field symptom.** With `PNEUMATIC_DATA_ADDR` pointing at a remote service — the
container topology the operator runbook documents — user reads went to a local
socket that had nothing on them, and resolved as stake 0. Consequences: role
selection installed no roles, and the registration gate rejected every peer. The
node logs a clean start and then does nothing. The existing runbook row
"Registration rejected for a peer with stake → verify in the data service" was
pointing operators at exactly this class of failure without naming it.

**Fix.** Implement `get_user`/`save_user` on the trait impl (delegating to
`get_data_internal`/`save_data_internal` like every sibling), delete the private
inherent duplicates. Regression guard:
`data-service/tests/service_roundtrip.rs::trait_dispatch_honors_the_configured_source_for_user_lookups`
holds the provider as `Arc<dyn DataProvider>` — the production shape — and
asserts the write lands in *the service it was pointed at*.

**Residual trap (open).** The trait still carries default bodies for
`get_token/save_token/get_data/save_data` that do the same
construct-a-fresh-provider trick. Any future provider that forgets to override
one inherits a silent endpoint swap rather than a compile error. Making those
methods required (no defaults) turns it into a compile-time failure; the blast
radius is the ~12 `impl DataProvider` sites, most of which already override them.
