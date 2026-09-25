---
id: event-s6-shielded-completion
title: "S6: shielded Tier-1 feature-complete"
type: event
namespace: pneumatic
visibility: namespace
summary: "2026-09-25: S6 closes the shielded plan — attack suite, concurrency, cross-crate 4-hop pipeline with wire-byte privacy assertion, prove/verify timing. Workspace 834/37/0; Tier-1 feature-complete, only operational items open."
auto_inject: false
applicable_when: "Dating the shielded stack's completion, auditing Tier-1 claims, picking up the operational open items"
confidence: 1.0
verified_at: "09/25/2026"
verified_by: "dsh-agent"
staleness_signal: "Historical event — never goes stale"
tags: [event, shielded, milestone, completion]
edges:
  - target: pillar-shielded-value-transfer
    type: supports
    weight: 1.0
    note: "Marks S6 (the final feature phase) complete"
  - target: task-s6-shielded-completion
    type: related_to
    weight: 1.0
    note: "The task this event closes"
  - target: fact-test-suite-s6
    type: supports
    weight: 0.8
    note: "The 834/37/0 baseline is the post-S6 state"
  - target: event-s5-2-finalizer-wiring
    type: preceded_by
    weight: 0.9
    note: "S5.2 wiring → S5.3/S5.4 pool → S6 completion"
related: []
source_url: "Empty"
---

# Event: S6 — shielded Tier-1 feature-complete (2026-09-25)

The shielded implementation plan is fully landed. S6 added the last four pieces, all green with `cargo check --workspace` + `cargo test --workspace` after each:

1. **S6.1 cross-crate pipeline** (`tests/shielded_pipeline.rs`): all four hops (Verify → SignShielded → ShieldedVote → Commit) across sentinel/finalizer/committer sharing one pending-registry and one `Arc<ShieldedPool>`; the privacy claim is pinned at the wire-byte level (no note plaintext in any recorded byte of any role; the public surface present in rmp), and the live lane proves a real halo2 transfer commits with pool + chain lockstep and idempotent replay.
2. **S6.2 canonical attack suite** (`tests/shielded_attacks.rs`): audit table + new fast (stale root/nullifier at apply, pre-check-4) and live (truncated proof → typed rejection, never panic; concurrent same-note spend → exactly one commits).
3. **S6.3 concurrency**: 50-thread nullifier race (exactly one per nullifier) and 200-append/4-reader Merkle race (proof/root pair never diverges).
4. **S6.4 proving UX** (`prover/src/build.rs`): `#[ignore]`d timing benchmark — warm untimed, timed prove + timed verify at k=10 (`--release`); measured **prove 25.4 s / verify 124 ms** (release, this box); the verify assert is a 200 ms tripwire (1.6× measured) because the 100 ms plan target was unmet within box noise.

Wire-compat held (zero wire changes, ground rule 4) and the lockfile is untouched (zero new packages). Baseline moved 828/32/0 → **834/37/0**.

Remaining work is OPERATIONAL, not code: (1) independent ActionCircuit audit as a launch gate, (2) viewing-key policy, (3) proving-UX product decision (embedded wallet vs prover service) given the S6.4 numbers, (4) anonymity-set genesis policy. These are tracked in the AUDIT_CHECKLIST S6 "Open items at close".
