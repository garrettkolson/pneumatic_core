---
id: log-implement-code-20261007-184500
type: log
operation: implement-code
date: "2026-10-07T18:45:00"
namespace: pneumatic
summary: "Closed roadmap Phase 0 with its last open exit test: the selection salt observed through the **production** `DefaultDataProvider` over a real socket to a real data service, mutation-verified three ways — chain lost on the wire, salt pinned to the genesis block, and the lookup failure swallowed into a default token, which is the original ADR-019 defect. Then made Phases 1–7 startable cold: a Live scorecard and a Pickup card per phase at the top of the roadmap, four stale claims in the rollout-start table corrected against the tree, and the vault's first playbook (`playbook-picking-up-a-roadmap-phase`) carrying the procedure and the traps that actually cost time. Suite 1155 to 1156/37/0."
affected_nodes: ["playbook-picking-up-a-roadmap-phase", "task-multihost-testnet-rollout", "fact-self-referential-quorum-denominator", "repo:sentinel/src/sentinel/tests/processing.rs", "repo:sentinel/src/sentinel/tests/helpers.rs", "repo:sentinel/Cargo.toml", "repo:data-service/src/server.rs"]
tags: ["log", "implement-code", "phase-0", "exit-test", "selection-salt", "data-service", "test-baseline", "roadmap", "vault-hygiene", "onboarding"]
---

Phase 0 had one exit test unmet, and it was the kind of gap Phase 0 existed to close: every
salt test drove `StubDataProvider`, so "the production provider returns a non-empty salt"
was an assumption carried into three later phases.

## The exit test

`selection_salt_through_the_production_data_provider` boots a real data service on an
ephemeral port and reads the salt through `DefaultDataProvider` — the same construction as
`tests/data_service_boot.rs` and `bin/node-server.rs`. To reach the sentinel's own
`selection_tip` (a `pub(crate)` method), the sentinel's test fixtures gained a
provider-agnostic entry point and a dev-dependency on `pneumatic_data_service`; the
typed-stub builders now delegate to it, so nothing existing moved.

Three properties, each mutation-verified by breaking it:

| Mutation | Result |
|---|---|
| The service stores a chainless token (simulating a blockchain lost across MsgPack) | fails on `the salt must be the stored chain's tip` — left `[]` |
| `chain_tip_of` pinned to block 0 instead of the tip | fails on `the salt must follow the chain` |
| `get_token(...).unwrap_or_default()` — the ADR-019 shape | fails on `a token that was never stored produced a salt (0 bytes)`; the stub-side test fails too |

The third was the one worth the exercise. The service answers a miss with an **empty
body** (`handle_get` → `unwrap_or_default()`), while the stub raises a real error — so the
two providers could have disagreed about misses, and a production-side default `Token`
would salt every selection with a constant forever while presenting as genesis. They
agree. That is now a tested fact rather than a hope.

## Making the remaining phases startable cold

The roadmap had strong analysis and no current state: a scorecard dated to rollout start,
a "first concrete action" that still told you to run Phase 0 fixes in parallel with it, and
phase bodies written as argument rather than as instructions. A new agent would have had to
re-derive which phases were untouched.

- **Live scorecard** at the top: status per phase, what is already in place, the first
  thing to do. Marked as superseding the rollout-start table where they disagree, with the
  four corrections named rather than silently overwritten.
- **Pickup card per phase**: read-first list, what the first commit looks like, the trap
  specific to that phase. Phases 1 and 2 are the only two worth starting today, and the
  cards say so — Phase 6 is explicitly blocked, Phase 7 is refused in code.
- **First playbook** (`playbook-picking-up-a-roadmap-phase`): the procedure (orient, then
  distrust in the right direction; verify anchors; find the live path before auditing a
  defect; reproduce before fixing; assert effects through the production path) and the
  traps that cost real time this week. It is the vault's first node of that type, so
  `playbook (0)` became `playbook (1)`.

## A stale claim the roadmap was about to propagate

The rollout-start table listed **"Slashing enforcement: Missing — `slash_fraction` appears
only at `CostModel` construction sites; no application site"**. It has two application
sites: an invalid chain's tip proposer (`committer/src/epoch_manager.rs:205-227`) and
double-sign resolution (`:262-275`), applied exactly once per op by a real
`StakingManager::apply_ops` (`:111-124`) and persisted with the epoch snapshot. Had the
scorecard been written from the table instead of the tree, Phase 4 would have carried a
build-this item for something that already ships. What Phase 4 genuinely lacks is the
*network* version of the exit test — slash a byzantine validator running as a second
committer process and read the reduced stake out of the snapshot the other roles load.

## Two things caught before they were written down

**A false finding about Phase 3.** A grep for `save_stake_snapshot` showed no production
call site, and the note was about to read "epoch snapshots are never persisted".
`advance_epoch_to` persists **both** stake and executor sets at
`committer/src/committer/epoching.rs:156-171`, surfacing a persistence failure as
`SnapshotPersist` rather than advancing anyway — the call is just wrapped across lines, so
the callee name never lands on one line. The rule went into the playbook: before writing
"never called", grep the trait method and read the functions that would call it. The
rollout-start table's "Validator-stake persistence: Missing — nothing writes it" was
corrected in place for the same reason.

**An edge-count habit.** INDEX rows carry an `edges` column that had drifted (a node with
6 edges listed 7; the roadmap with 13 listed 9). The counts for every row touched here now
come from the file, and the rebuild note says so — a counter nobody can reproduce is worse
than no counter.

## A vault-hygiene item for the next agent

Validating every edge target and `[[wikilink]]` against the titles on disk turned up **15
dangling links**, 14 of them pre-existing: `related:` wikilinks naming a node's *old* title
after a rename, and three pointing at node ids that were never created
(`fact-control-plane-peering`, `fact-data-service`). One was mine from earlier today —
ADR-019 pointing at "…not the set that was assigned" when the title reads "…not **by** the
set that was assigned", now fixed.

The typed `edges:` are clean; only `related:` drifts. Worth a sweep with
`infinite-brain-vault-health`, and a caution when writing one: a `related:` entry is a
**title**, so renaming a node silently breaks it, while an `edges:` target is an **id** and
survives the rename. That asymmetry is why this class of defect recurs.

## State

Suite **1156 passed / 37 ignored / 0 failed**. Vault at 147 nodes (147 INDEX rows, counted
from the file). Phase 0 closed; Phase 1 is next and needs a decision before code.
