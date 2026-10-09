---
id: fact-token-chain-window-caps-delivery-gate
title: "A token's chain is a five-block sliding window, so a chain-length gate stops being a delivery measure"
type: fact
namespace: architecture
visibility: namespace
summary: "10/08/2026, caught by the rehearsal's own exit test. `Token::security_level` (src/tokens.rs:46, `DEFAULT_SECURITY_LEVEL = 5`) doubles as the maximum chain length: `has_reached_max_chain_length()` is `security_level == blockchain.get_count()` and `Token::commit_block` calls `remove_oldest()` once it is reached (src/tokens.rs:288). So for any default token the count is capped at 5 and every later commit trims as it appends — `traffic.sh`'s 'chain grew by N' gate became unsatisfiable the moment the window filled: a 20-tx run reported 'grew by 0 of 20 — NOT DELIVERED' (exit 1) while the committer's log kept printing COMMIT-OK and the tip moved (00a24042… → f2e50052… across a 5-tx run with the count pinned at 5). The monotonic field is `Token::sequence_number`, bumped exactly once per committed block and never on a trim — but it counts APPENDS, so a conflict-resolution replacement moves it too (a 20-tx run moved it by 24). Beyond the harness: a healthy chain and a stalled one looked identical to the instrument, which is the failure mode Trap #1 warns about in measurement form."
auto_inject: false
applicable_when: "Writing or reviewing any assertion that reads chain length as progress; sizing a rehearsal's batch against a token's window; choosing a field to measure delivery; reasoning about how much history a non-archiver node keeps"
confidence: 1.0
verified_at: "10/08/2026"
verified_by: "dsh-agent"
staleness_signal: "If `security_level` stops doubling as the trim threshold, or `Token::commit_block` stops trimming; if the rehearsal genesis grows a way to set a window larger than a batch (then per-transaction observability becomes assertable and `sequence` can be demoted to a floor); if anything else in the tree reads `get_count()` as a progress or replay signal"
tags: [fact, tokens, blockchain, pruning, measurement, multihost, rehearsal, exit-test]
edges:
  - target: fact-mesh-verification-probe
    type: related_to
    weight: 0.8
    note: "Same discipline from the other side: a green probe proves formation only, and a capped counter proves nothing about delivery — both are instruments whose scope must be stated"
  - target: fact-composite-per-host-fork-divergence
    type: related_to
    weight: 0.85
    note: "Why the replacement appends exist at all, and why the commit counter is a floor rather than a per-transaction proof"
  - target: task-multihost-testnet-rollout
    type: related_to
    weight: 0.9
    note: "Phase 2's exit test is 'the chain grows', and this is the quantity that quietly stopped moving"
---

# The chain counter stops moving at five blocks

## Mechanism

`security_level` is documented as "number of confirmations needed before trimming
old blocks", and it doubles as the chain-length ceiling:

```rust
// src/tokens.rs:305
pub fn has_reached_max_chain_length(&self) -> bool {
    self.security_level == self.blockchain.get_count()
}
```

`Token::commit_block` consults it before appending — `if !is_archiver &&
self.has_reached_max_chain_length() { self.blockchain.remove_oldest(); }`
(`src/tokens.rs:288`) — and `DEFAULT_SECURITY_LEVEL` is **5** (`src/tokens.rs:46`).
The rehearsal's transaction token is seeded by the data service with
`Token::new()`, so it inherits the default. Every commit past the fifth trims the
oldest block and appends: the chain advances, the count does not.

Archivers are exempt (`!is_archiver`), which is the intended shape — history
lives with the archiver role, and a non-archiver holds a window.

## The gate that could never pass again

`deploy/multihost/traffic.sh` submits N transactions and asserts the committer's
chain grew by N through the data service. First run after a bring-up: `0 → 5` for
five transactions — pass. Second run: **20 transactions, "chain grew by 0 of 20 —
NOT DELIVERED", exit 1** — with the window already full. The pipeline was fine;
the instrument had run out of range.

The trap is not the false negative alone. A count that cannot move makes a dead
pipeline and a working one **indistinguishable** — the shape Trap #1 of the
rollout warns about, in measurement rather than control-plane form.

## What measures delivery instead, and what it does not

`Token::sequence_number` is bumped exactly once per committed block and never on
a trim, so it is monotonic in commits regardless of the window. It is now printed
by `examples/read_token_chain` (`blocks=` / `sequence=` / `tip=`) and is what
`traffic.sh` gates on, with a negative control: stop `pmesh-committer-1` and three
submissions report "0 of 3 submissions reached a committed block", exit 1.

What it does **not** give: per-transaction proof. It counts *appends*, and a
conflict-resolution replacement (`rollback_tip_hash`) is an append — a 20-tx run
moved it by 24. And with a 5-block window, a batch larger than 5 cannot be checked
by transaction id from outside the node at all. A rehearsal that wants that needs
a genesis-settable window larger than its batch, or an archiver to read from.

## Adjacent harness fact, recorded here because it compounds

Each data service re-applies `genesis.json` at boot, so token state — chain and
sequence — resets on every `./up.sh`. Delivery numbers are therefore per-bring-up
and a re-run silently starts from an empty chain. Nothing in the harness says so
out loud; this is also why the first run of a session reads `before: 0`.
