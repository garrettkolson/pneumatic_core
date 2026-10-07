---
id: playbook-picking-up-a-roadmap-phase
title: "Picking up a roadmap phase cold: the procedure, and the traps this repo actually has"
type: playbook
namespace: pneumatic
visibility: namespace
summary: "The working procedure for starting any phase of `task-multihost-testnet-rollout` with no prior context: orient in the vault, verify every anchor, find the live path before auditing a defect, reproduce before fixing, and leave the vault truer than you found it. Plus the traps that cost time in practice — `cargo test` halts at the first failing target so totals read short; the workspace suite exceeds the default command timeout; an absence-grep misses a call wrapped across lines; adding a field to `Config`/`EnvironmentMetadata` costs ~19 exhaustive literals that `cargo check -p` cannot see; the committer has no `log` crate; and a whole-tree regex to fix N files silently edits 185."
auto_inject: true
applicable_when: "Starting, continuing, or reviewing work on any roadmap phase; auditing a defect the vault already names; running or reporting the test suite; editing shared config structs; or committing in this repository"
confidence: 0.9
verified_at: "10/07/2026"
verified_by: "dsh-agent"
staleness_signal: "If the baseline suite count has moved; if `cargo test --workspace --no-fail-fast` no longer sums every target; if the committer gains a logging dependency; if a trap listed here turns out not to be true"
tags: [playbook, process, roadmap, testing, vault-protocol, hygiene, onboarding]
edges:
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.95
    note: "The phases this procedure applies to; its Live scorecard and Pickup cards are the phase-specific half"
  - target: fact-committer-confirmation-gate-did-not-gate
    type: supports
    weight: 0.9
    note: "What happens when a defect is audited at the function the roadmap named instead of the live one: three silent failures went unfound for a day"
  - target: fact-finalizer-reassignment-never-delivered
    type: supports
    weight: 0.85
    note: "Why every 'was rejected' claim here needs a mutation: four independent layers each would have made a dead send look like a working one"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.8
    note: "The standard the exit tests are written against — a refusal must be distinguishable from a success"
related: ["[[Multi-host testnet rollout: from a working local cluster to production]]", "[[The one stake-weighted quorum an ordinary transaction passes did not actually gate: claims were obeyed, self-votes never counted, early votes dropped]]"]
source_url: "Empty"
---

# Picking up a roadmap phase cold

Written 10/07/2026 after three phases' worth of work in one day, two of which started from
a cold read. Every trap below is one that cost time, not one that sounds general.

## The procedure

**1. Orient in the vault, then distrust it in the right direction.**
`_system/INDEX.md` → the phase's Pickup card in `task-multihost-testnet-rollout` → the
linked fact nodes. A node tells you *what was found and where*; it does not tell you what
is true now. When a node and the tree disagree, the tree wins and the node is a bug.

**2. Verify every anchor before acting on it.**
Vault and roadmap citations are `file:line` and they drift the moment anyone edits the
file. Two citations in the roadmap were stale within 48 hours of being written. Re-open
the file at the line; if it isn't the cited thing, grep for the identifier.

**3. Find the live path before auditing a defect.**
The single most expensive mistake of the session: a roadmap item described a defect in
`reconcile_signatures`, the arithmetic was correct, and the function it lived behind had
**no caller**. The live standard-token path is the optimistic one, so the real gate was a
different gate — which had three more silent failures behind it. Ask *"who calls this?"*
before *"how is this wrong?"*. If the answer is nobody, you have found something more
interesting than the bug you came for, and the roadmap item is now wrong too.

**4. Reproduce before you fix; mutation-verify after.**
Every test here that asserts "was rejected", "was dropped", or "never ran" is worth
nothing until you have watched it fail with the fix reverted. The routine: back up the
file, revert the one line or one arm, run the single test, confirm it fails **on the
assertion you expected** (a failure on a different assertion means the test is pinned to
something else), restore from the backup, and `diff` against the backup to prove the
restore was byte-exact. This catches both directions: tests that pass for the wrong
reason, and edits that quietly landed in the wrong file.

**5. Assert the effect through the production path.**
Stubs answer from a map they built themselves, so they cannot disagree with their caller
about misses, empty values, or round-trips. `selection_salt_through_the_production_data_provider`
exists because "the salt is non-empty" was provable against a stub while the wire could
still have been losing the blockchain. Same for sends: assert the counter, not the return
value — a `Result` nobody inspects and a `bool` nobody read are both silent.

## The traps, specifically

**`cargo test` halts at the first failing target.**
A failure in one crate's test binary stops the run, so the printed totals look short by
dozens and read like a regression that isn't there. Use `cargo test --workspace
--no-fail-fast` and sum the `test result:` lines. Current baseline: **1156 passed / 37
ignored / 0 failed** (10/07/2026). `test.log` and `test_log.txt` are untracked by design.

**The workspace suite exceeds the default command timeout.**
Run it in the background and do other work; a foreground run gets killed mid-suite and
the partial output looks like a failure.

**An absence-grep is not evidence.**
"Nothing ever persists stake snapshots" is what a grep for `save_stake_snapshot` suggested
— and `advance_epoch_to` does exactly that, with the call wrapped across lines so the
callee name never appears on one line. Before writing a "never called / never written"
claim down, grep the *trait method*, then read the two or three functions that would
plausibly call it. A false finding in the vault costs the next agent more than the hour it
saved you.

**Shared config structs are wider than they look.**
Adding a field to `Config` or `EnvironmentMetadata` means ~19 exhaustive struct literals
across the workspace, and `cargo check -p <crate>` sees only its own crate. Always finish
with a workspace-level check when a shared type changed. Deleting a field is cheaper than
adding one, and there is no `deny_unknown_fields`, so an old `env.json` keeps loading —
which is what made deleting `shard_quorum_percentage` safe.

**Committer has no `log`/`tracing` dependency.**
In `pneumatic_committer`, log via `self.env_data.logger.log(format!(...))`
(`Logger::log(&self, String)`), and log **once per event of interest, not once per
message** — a per-vote line in a quorum path is a self-inflicted DoS on your own log
file. The core crates use `tracing`; the two are not bridged deliberately.

**Never run a whole-tree regex to fix N files.**
A regex intended to repair dangling commas in a handful of JSON fixtures rewrote **185
files**, because Rust tolerates trailing commas before a closing brace and absorbed the
damage invisibly — no compile error, no failing test, just 1,600 lines of diff noise on
code nobody was editing. If it happens anyway, recover by proof rather than by hope:
apply the same transform to each file's `git show HEAD:<path>` blob and keep only the
files where the result matches the working copy — those provably contain nothing but
sweep damage. That identified 167 files to restore and, usefully, *proved the set of files
holding real work was complete*. Never `git checkout .` in a tree that holds uncommitted
work.

**Capacity refusals that report only a `bool`.**
`register_peer` is capped by `get_max_node_number` and signals refusal as `false`. A test
fixture that "registers all six candidates" may register almost none and still pass,
because nothing reads the return value. Assert the refusal: `assert!(register_peer(...))`.

**Dead code the design still claims is not cleanup.**
`try_finalize` is unwired, but ADR-005 keeps the quorum machinery as the conflict-resolution
path and it is that machinery's only implementation. It stays, with a doc comment saying it
has no caller. Wire it or remove it deliberately; never sweep it up as a side effect of
something else.

## Finishing a phase

A phase is done when its exit test is asserted through the production path and
mutation-verified; the suite is green at workspace scope; the vault says what is now true
rather than what was planned; and every residual is named — an unclaimed gap in a node is
how a future agent ends up re-auditing a phase that looked finished.

Vault duties on landing, in order: correct or add the affected nodes (a *corrected* node
keeps its wrong claim visible with the correction on top — that is what makes the next
audit faster), keep `_system/INDEX.md` in sync **counting from the file**, and append one
log node. One commit for code, one for the vault, with the message recording *how* each
claim was checked, because "fixed" without a method is what this vault keeps having to
re-derive.
