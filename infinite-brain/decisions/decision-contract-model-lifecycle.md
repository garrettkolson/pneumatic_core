---
id: decision-contract-model-lifecycle
title: "ADR-014: Contract model and lifecycle (on-chain deploy, Model X calls)"
type: decision
namespace: pneumatic
visibility: namespace
summary: "Contract ≡ contract token; on-chain deploy (deterministic token id); public execution; Model X snapshot-pinned non-atomic cross-contract calls; M-of-N multisig + 1-epoch timelock upgrades."
auto_inject: false
applicable_when: "Deploying, upgrading, or calling contracts; designing cross-token interactions or contract governance"
confidence: 0.9
verified_at: "09/27/2026"
verified_by: "Garrett Olson"
staleness_signal: "Stale when contracts stop being 1:1 with tokens, deployment returns to admin-only, calls become atomic two-phase, or governance moves to stake-weighted voting"
tags: [adr, design-decision, contract-model, deployment, cross-contract-calls, governance]
edges:
  - target: concept-executor-role
    type: related_to
    weight: 0.9
    note: "The executor runs deployment (protocol op) and cross-contract call frames"
  - target: concept-per-token-chains
    type: related_to
    weight: 0.85
    note: "Each contract is its own lattice chain; calls span chains via snapshot refs"
  - target: concept-optimistic-finality
    type: related_to
    weight: 0.85
    note: "Model X was chosen specifically to preserve first-signature finality for calling txs"
  - target: decision-executor-sharding
    type: related_to
    weight: 0.75
    note: "Snapshot pinning keeps call frames deterministic across A's shard members"
  - target: pillar-block-lattice
    type: related_to
    weight: 0.7
    note: "The contract model is an extension of the per-token lattice, not a new chain layer"
  - target: task-executor-contract-execution
    type: supports
    weight: 0.8
    note: "Bounds Phases 5-7 of the implementation plan"
related: []
source_url: "plans/executor-contract-execution-implementation-plan.md (Q6)"
---

# ADR-014: Contract model and lifecycle (on-chain deploy, Model X calls)

Approved by Garrett Olson, 09/27/2026.

- **C1 — Ontology: contract ≡ contract token (1:1).** One contract = one token = one
  lattice chain; contract state = the token's `asset_data` (bytecode) + that
  token's account state. Rejected: many contracts per token (EVM-style account
  model) — the pipeline is partitioned per-token end to end.
- **C2 — On-chain deployment.** A `DeployContract` tx creates a contract token with
  no admin step; `token_id = H(deployer_pubkey ‖ deployer_nonce ‖ bytecode_hash ‖
  name)` is derivable pre-execution (replay-proof via `User.nonce`). Deployment is a
  protocol op: the executor emits a `CreateToken` delta (ADR-013) applied at commit.
  Rejected: admin-gated off-chain genesis.
- **C3 — Public execution.** Anyone may send a tx to any registered contract token
  (`token_id` + entry-point `action` + `payload`); effects are constrained by C6.
  `ProxyAuthorization` (`src/tokens.rs:504-510`) remains the future inter-contract
  authorization primitive.
- **C4 — Cross-contract calls: Model X (snapshot-pinned, non-atomic).** A call from
  contract A to contract B carries an explicit snapshot ref `B@(height, hash)`; the
  executor reads B at that pinned ref (deterministic across A's shard members); B's
  side is a cross-referenced tx on B's chain. A finalizes on its own chain under
  optimistic finality; a B-side failure settles via a deterministic
  revert/compensation tx on A's chain. Rejected: two-phase atomic commit — it
  withholds A's finality until B's finalizes, killing first-signature finality for
  exactly the tx class being added.
- **C5 — Upgrades: multisig + timelock.** Contracts register N owner keys; an
  upgrade tx (new bytecode → new `asset_data`/`asset_hash`) needs M-of-N owner
  signatures + a one-epoch delay before apply. Stake-weighted voting is a documented
  future extension. Rejected: admin-gated upgrades; full on-chain voting.
- **C6 — Effect scope.** A contract may only move units of *its own* token (deltas
  reference that token's accounts; no negative balances, checked at apply; fuel
  untouched).

Follow-on design ADRs (written at the start of their phases): **ADR-015** on-chain
deployment mechanics, **ADR-016** cross-chain call semantics (full Model X design),
**ADR-017** upgrade governance.
