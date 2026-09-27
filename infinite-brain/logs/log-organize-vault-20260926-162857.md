---
id: log-organize-vault-20260926-162857
type: log
operation: organize-vault
date: 2026-09-26T16:28:57
namespace: pneumatic
summary: "README.md trimmed 690 → 252 lines (contributor-focused, current state verified against code + a live cargo test --workspace run: 835/37/0). ADR-001…010 migrated out of the README into 9 first-class decision nodes (005+010 merged); state machine and quorum-gossip minutia moved to 2 concept nodes; new dated roadmap note supersedes the 09/20 snapshot. Fixed the executor's stale 'Execute' outbound claim (code-verified 'Sign' at executor.rs:495). INDEX.md rebuilt to 94 nodes."
affected_nodes: ["source-readme-adrs", "note-roadmap-status-2026-09-20", "note-roadmap-status-2026-09-26", "hyp-readme-postmvp-phase-8-outstanding", "fact-test-suite", "fact-worker-crate-tests", "concept-executor-role", "concept-optimistic-finality", "decision-trait-based-abstraction", "decision-dashmap-registries", "decision-deterministic-leader-election", "decision-deterministic-per-tx-routing", "decision-optimistic-finality", "decision-stake-snapshots-in-dataprovider", "decision-sentinel-routing-authority", "decision-conflict-definition-and-resolution", "decision-executor-sharding", "concept-transaction-state-machine", "concept-quorum-gossip-protocol", "_system/INDEX"]
tags: ["log", "organize-vault"]
---

# Log: organize-vault — README trim + ADR migration to decision nodes

Per the standing vault protocol, the README was trimmed and its design content moved into the vault:

**README (690 → 252 lines).** Kept/added for contributors: quick start (current test counts), 7-crate workspace map, key dependencies (incl. halo2_proofs, pqcrypto-*, rns-* pins), 23-module map + sub-packages, node roles, consensus flow + state machine, hybrid PQ crypto, wire protocol + wire-actions table, configuration, shielded Tier-1 overview, composite runtime, development conventions, roadmap status summary. Removed: the ADR section (→ vault), the Phase 0–10 roadmap tables (→ TASKS.md + vault notes), sub-crate component tables (already in the role concept nodes).

**New nodes (12):** `decision-trait-based-abstraction` (ADR-001), `decision-dashmap-registries` (ADR-002), `decision-deterministic-leader-election` (ADR-003), `decision-deterministic-per-tx-routing` (ADR-004), `decision-optimistic-finality` (ADR-005+010 — the README recorded the decision twice), `decision-stake-snapshots-in-dataprovider` (ADR-006), `decision-sentinel-routing-authority` (ADR-007), `decision-conflict-definition-and-resolution` (ADR-008), `decision-executor-sharding` (ADR-009); `concept-transaction-state-machine`, `concept-quorum-gossip-protocol` (minutia removed from the README); `note-roadmap-status-2026-09-26` (supersedes the 09/20 snapshot: S5.3/S5.4/S6 all landed, Tier-1 feature-complete).

**Updated nodes (8):** `source-readme-adrs` (ADRs promoted, section removed), `note-roadmap-status-2026-09-20` (marked superseded + followed_by edge), `hyp-readme-postmvp-phase-8-outstanding` (re-anchored to TASKS.md; ADR half of Phase 8 now satisfied by the vault nodes), `fact-test-suite` + `fact-worker-crate-tests` (09/26 verified baseline 835/37/0; per-crate: core 548/19, committer 92+9/7, executor 10, finalizer 61, node-server 32/2, prover 15/2, sentinel 57/1), `concept-executor-role` (outbound is a `"Sign"` vote, not `"Execute"` — code-verified at executor.rs:495), `concept-optimistic-finality` (edge → quorum gossip protocol).

**INDEX.md:** rebuilt — decision 5→14, concept 32→34, note 2→3; total 82→94 nodes.

**Verification:** `cargo test --workspace` (live run, 09/26): 835 passed / 37 ignored / 0 failed. Note: CLAUDE.md's module/architecture table (20 modules, 5 crates) remains stale — out of scope for this pass, flagged for a future update.
