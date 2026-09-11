# Pneumatic Shielded Transactions — Implementation Roadmap

**Purpose of this document**: You (the implementing agent) are being asked to add
Zcash-style privacy — hidden sender, receiver, and amount — to the Pneumatic
protocol. This document gives you the context, the design decisions already
made, the reasoning behind them, a phased task breakdown, and the open
problems you should expect to hit. Read the whole thing before writing code;
the design decisions in Part 2 constrain almost everything in Part 4.

Repo: https://github.com/garrettkolson/pneumatic_core

---

## Part 0: What "done" looks like

A user can send a token transfer where the network confirms it is valid
(correct signatures of ownership, no double-spend, balances reconcile) without
any node — Sentinel, Executor, Finalizer, or Committer — ever seeing who sent
it, who received it, or how much moved. A blockchain observer sees only:
opaque commitments, nullifiers, and a proof that everything checks out.

Two tiers of ambition, and you should treat them as separate projects with a
hard boundary between them:

1. **Tier 1 — Shielded value transfer.** Private transfers of self-signed
   tokens. This is the Zcash-equivalent problem: bounded, well-understood,
   solvable with existing tooling. **This is the actual deliverable for a
   first implementation.**
2. **Tier 2 — Private contract execution.** Hiding the *logic and state* of
   arbitrary executed-token contracts, not just value transfer. This is
   zk-rollup / zk-VM territory (comparable to what Aztec, Aleo, or Mina are
   doing). It is a multi-year research-grade effort. **Do not attempt to
   design this in detail up front** — Part 6 explains why and what to do
   instead.

If you are an agent being handed this document with a deadline, assume Tier 1
unless explicitly told otherwise.

---

## Part 1: Current architecture (context you need before changing anything)

Pneumatic is a Rust proof-of-stake blockchain for distributed worker-node
networks, using a block-lattice-style design (conceptually similar to Nano —
each token has its own append-only chain rather than one global chain).

**Workspace layout**:
```
pneumatic_core/
├── src/            # pneumatic_core crate — 21 core modules
├── sentinel/       # pneumatic_sentinel — validation & routing
├── executor/       # pneumatic_executor — contract execution
├── finalizer/      # pneumatic_finalizer — quorum & block building
├── committer/      # pneumatic_committer — chain commitment & epochs
```

**Node pipeline today**:
```
Sender → Sentinel → ─────────────────────────────────→ Committer
              │
              ├─ SelfSigned token: Sentinel validates → Committer directly
              │  (skips Executor + Finalizer)
              │
              └─ Standard/executed token:
                  Sentinel → Executor (preload + execute + hash) →
                  Finalizer (collect sigs → quorum → build block) →
                  Committer (commit block to chain)
```

**Key modules you will touch**:

| Module | Current responsibility | Why it matters here |
|---|---|---|
| `crypto` | `AsymCryptoProvider` (Ed25519 sign/verify, hybrid AES-256-GCM + X25519 encryption), `HashProvider` (SHA-256) | You're adding a proving system and a SNARK-friendly hash here. The existing hybrid encryption is reusable almost as-is for note encryption. |
| `tokens` | `Token`, `BlockValidator` trait, `TokenFactory`, `Blockchain` | `Token`/`Blockchain` are per-token chains — this is the block-lattice structure that has privacy implications (see Part 2.4). |
| `transactions` | `Transaction`, `SignedTransaction`, `TransactionCommit`, `TransactionPool`, explicit state machine (`Pending → Preloaded → Validated → Executing → Finalizing → Committed`) | New shielded transaction variant goes here. |
| `validation` | `TransactionValidationSpec` / `BlockValidatorSpec` traits, `SelfSigned`/`Executed` specs, spec registries | This pluggable spec system is your friend — a new `ShieldedValidationSpec` slots in without touching the core validation dispatch logic. |
| `epoch` | `LeaderSelector`, `EpochBoundaryDetector`, `resolve_block_conflict()` | Largely unaffected — leave alone unless a design decision below says otherwise. |
| `registry` | `PendingTransactionRegistry`, `TransactionSignatureRegistry` | Needs a new companion: a nullifier-set registry (Part 4.5). |
| `sentinel` crate | Gatekeeper — validates, routes | Gains a "verify proof, don't execute" path. |
| `executor` crate | Fetches data, runs contract logic, hashes result | For Tier 1, shielded transactions **skip this entirely**. |
| `finalizer` crate | `SignatureCollector`, `BlockBuilder`, `MessageDispatcher` | Signs over public outputs + proof validity instead of plaintext execution results. |
| `committer` crate | `Committer`, `BlockServices`, `StakeStore`, `LeaderSelector` | Gains commitment-tree and nullifier-set persistence. |

**Wire protocol**: 4-byte big-endian length header + MsgPack payload. This
does not need to change structurally — proofs and commitments just become
new fields serialized the same way.

---

## Part 2: Design decisions (made — do not re-litigate without strong reason)

### 2.1 Proving happens client-side, not on the network

This is the single most important decision and it drives everything else.
Zcash's model: the *sender's wallet* builds the zk-SNARK proof off-chain
before ever submitting the transaction. Miners/validators only **verify** the
proof — they never re-execute anything.

Pneumatic's Executor currently needs plaintext transaction data to run
contract logic, and the Finalizer needs to see what it's signing to reach
quorum. Both of those are incompatible with a network that hides
transaction contents *unless proving moves off-network*. Once it does, the
entire Executor stage becomes unnecessary for shielded transfers, and the
Finalizer's job gets *simpler*, not harder — it's now signing over a
proof-verification result (boolean) plus public outputs, which is
deterministic and cheap to check, rather than adjudicating execution
results.

**Implication**: you need a new client-side component — a wallet/SDK that
holds the proving key and constructs proofs. This does not live in any of
the four node crates. Plan for a new crate, e.g. `pneumatic_prover`.

### 2.2 Proving system: Halo2

Recommended over Groth16/BN254 (the older Zcash Sapling approach) for two
reasons:
- **No trusted setup.** Groth16 requires a per-circuit trusted setup
  ceremony (like Zcash's original "powers of tau"). Halo2 (used by Zcash's
  current Orchard shielded pool) has a transparent/universal setup — no
  ceremony, no toxic waste to worry about, which matters more for a
  from-scratch chain than an established one.
- **Rust-native**, fits directly into this codebase's existing Rust/Cargo
  workflow without FFI to a C++ proving library.

Tradeoff to accept: Halo2 proofs are somewhat larger and slower to verify
than Groth16. For a client-side proving model this is an acceptable
tradeoff — proving time (seconds, done once by the sender) matters more
than verification time (milliseconds, done many times by the network), and
Halo2 verification is still fast.

Crate to integrate: `halo2_proofs` (or `halo2curves` + `halo2_gadgets`
depending on how much of the Orchard gadget library you want to reuse
rather than write from scratch — reusing Orchard's note/commitment gadgets
where possible will save months).

### 2.3 State model: note-based (UTXO-like), not shielded account balances

Two ways to represent private value:
- **Note-based (Zcash/Orchard style)**: value lives in discrete "notes,"
  each a commitment to (value, owner, randomness). Spending a note reveals
  a nullifier (derived from the note + owner's spend key) that proves it's
  being spent without revealing which note. New notes are created as
  commitments inserted into a Merkle tree.
- **Shielded account model**: a single encrypted balance per account,
  updated via homomorphic operations. Used by protocols like Zether. Harder
  to get right (concurrent-update race conditions, replay issues) and less
  mature tooling.

**Decision: note-based.** It's the better-trodden path, maps cleanly onto
Pneumatic's existing self-signed-token transactions (which are already
transfer-shaped, not stateful-account-shaped), and lets you reuse the
Orchard circuit design as a reference implementation almost directly.

### 2.4 Pool structure: one shared shielded pool, not per-token shielding

Pneumatic's block-lattice design gives each token its own chain. This is
great for auditability but terrible for anonymity if applied naively to
shielded value — a small anonymity set per chain leaks almost as much as no
shielding at all (an observer can still see *which chain* moved, even if
not by how much or to whom).

**Decision**: build **one global shielded pool** (a single Merkle
commitment tree + one nullifier set) that any token opting into privacy
routes through, analogous to Zcash's single `z-addr` pool sitting alongside
transparent `t-addr` balances. Transparent per-token chains continue to
exist for tokens that don't need privacy. Do not attempt to shield
individual per-token lattice chains — you will end up rebuilding a shared
pool badly, or shipping something with a negligible real anonymity set.

### 2.5 Nullifier set is a new consensus-critical data structure

There is currently nothing in Pneumatic playing this role. The nullifier
set must be:
- **Append-only and durably committed** — same durability guarantees as
  the block-lattice chains today, because a rollback that "un-spends" a
  nullifier is a double-spend vulnerability.
- **Globally agreed** — all Committers need a consistent view, so its
  update path goes through the same epoch/quorum machinery as blocks do.

Plan this as a first-class structure in the `committer` crate from day one,
not a bolt-on `HashSet`.

### 2.6 Compliance/audit escape hatch: viewing keys

Recommend including this from the start rather than retrofitting it —
Zcash added viewing keys after the fact and it was disruptive. A **viewing
key** lets a note's owner (or an authorized third party, e.g. for tax or
audit purposes) decrypt the note's contents without being able to spend it.
This is a config/product decision, not just a technical one — flag it to
whoever owns the product requirements rather than deciding unilaterally.

---

## Part 3: Reading list (read before implementing, not instead of this doc)

- **Zcash Protocol Specification** — the canonical reference for
  commitment/nullifier construction, note structure, and the shielded
  transaction lifecycle. Even though Pneumatic isn't UTXO-based today, this
  is the closest existing production design to Tier 1.
- **Zcash Orchard** (the current Halo2-based shielded pool, successor to
  Sapling) — closest match to the proving-system decision above. Its
  gadget library is a strong reference for circuit design.
- **Halo2 book** (`zcash/halo2` docs) — proving system mechanics,
  circuit-writing patterns, the `Chip`/`Gadget` abstraction.
- **Aztec Protocol** docs — for Part 6 (Tier 2) context on private
  general-purpose contract execution. Do not attempt to replicate their
  full design for a first implementation, but it's the most relevant prior
  art if Tier 2 is ever scoped seriously.
- Pneumatic's own `TASKS.md` and `CLAUDE.md` in the repo root — house
  conventions for task breakdown and development guidance.

---

## Part 4: Phased task breakdown (Tier 1 — shielded value transfer)

Follows the same phase/task/estimate format as the existing `TASKS.md` so it
merges cleanly into the project's existing planning docs.

### Phase S1: Cryptographic primitives (blocks everything else)

| Task | Description | Estimate |
|---|---|---|
| Integrate Halo2 | Add `halo2_proofs`/`halo2curves` to workspace; confirm it builds cleanly alongside existing `ed25519-dalek`/`ring`/`aes-gcm` deps | 4h |
| Poseidon hash | Add a SNARK-friendly hash (Poseidon) to `crypto` alongside existing SHA-256 `HashProvider` — needed for in-circuit hashing of commitments/nullifiers | 8h |
| Commitment scheme | Implement note commitment: `commit(value, owner_pk, rho, rcm) -> Commitment`, following Orchard's note structure as a reference | 12h |
| Nullifier derivation | Implement `nullifier(note, spend_key) -> Nullifier`, deterministic and unlinkable to the commitment without the spend key | 8h |
| Note encryption | Adapt existing X25519 + AES-256-GCM hybrid encryption in `crypto` to encrypt note plaintext (value, memo) to the recipient's public key | 6h |
| Merkle tree impl | Append-only incremental Merkle tree over note commitments, with efficient root computation and membership-proof generation | 16h |

**Sub-total**: ~54h / ~1.5 weeks

### Phase S2: Circuit design

| Task | Description | Estimate |
|---|---|---|
| Spend circuit | Prove: (a) knowledge of a note opening for a commitment in the tree, (b) correct nullifier derivation, (c) knowledge of spend authority — without revealing which note | 40h |
| Output circuit | Prove: new commitment is well-formed or fold into a joint spend+output circuit (Orchard uses a combined "Action" circuit — recommend following that pattern rather than separate spend/output circuits) | 24h |
| Balance/value-balance constraint | Prove sum(inputs) = sum(outputs) + fee, without revealing individual amounts (Pedersen commitments to values, homomorphic sum check) | 16h |
| Circuit test vectors | Build a test harness with known-good witnesses/proofs for regression testing before wiring into the pipeline | 12h |

**Sub-total**: ~92h / ~2.5 weeks — **this phase carries the most schedule
risk in Tier 1.** Circuit bugs are subtle and expensive to find late; budget
extra review time here rather than compressing it.

### Phase S3: Wire format & transaction types

| Task | Description | Estimate |
|---|---|---|
| `ShieldedTransaction` type | New variant alongside existing `Transaction`/`SignedTransaction` in `transactions` module: carries nullifiers, new commitments, Merkle root reference, and the proof bytes | 8h |
| MsgPack serialization | Wire format for proof + commitment data over the existing 4-byte-length-prefixed framing — no framing changes needed, just new payload schema | 4h |
| `pneumatic_prover` crate | New client-side crate: holds proving key, exposes `build_shielded_tx(inputs, outputs, spend_keys) -> ShieldedTransaction` | 20h |

**Sub-total**: ~32h / ~4 days

### Phase S4: Validation layer

| Task | Description | Estimate |
|---|---|---|
| `ShieldedValidationSpec` | Implement `TransactionValidationSpec` — verify the proof against public inputs (nullifiers, new commitments, Merkle root); register via existing `ValidationSpecRegistry` | 12h |
| Nullifier-set registry | New DashMap-backed (or equivalent) registry in `registry` module for nullifier CRUD + double-spend checks, mirroring `PendingTransactionRegistry`'s concurrency patterns | 10h |
| Merkle-root freshness check | Reject proofs referencing a stale root beyond an acceptable window (root only advances as new commitments land) | 6h |

**Sub-total**: ~28h / ~3.5 days

### Phase S5: Pipeline rewiring

| Task | Description | Estimate |
|---|---|---|
| Sentinel: shielded routing | Add a routing branch: shielded transactions go straight to proof verification, bypassing preload/Executor dispatch entirely | 8h |
| Finalizer: sign over public outputs | `SignatureCollector` collects signatures over (proof-valid boolean, new commitments, nullifiers, new root) instead of plaintext execution results — `BlockBuilder`/`MessageDispatcher` need corresponding field changes | 12h |
| Committer: commitment tree persistence | `Committer`/`BlockServices` gain the shared shielded pool's Merkle tree and nullifier set as first-class committed state, updated per confirmed block | 20h |
| Epoch integration | Confirm `resolve_block_conflict()` and epoch boundary logic behave correctly when blocks contain shielded-pool updates (should be largely unaffected — verify, don't assume) | 8h |

**Sub-total**: ~48h / ~1 week

### Phase S6: Testing & hardening

| Task | Description | Estimate |
|---|---|---|
| End-to-end integration test | Full pipeline: client builds proof → Sentinel verifies → Finalizer quorum → Committer updates tree/nullifier set → confirm balance conservation | 16h |
| Double-spend attack tests | Attempt to replay a spent nullifier, replay against a stale root, submit malformed proofs — confirm all rejected | 12h |
| Concurrency stress tests | Concurrent shielded transactions hitting the nullifier-set registry, following existing `registry.rs` concurrent-test conventions (`std::thread::spawn` + `Arc`-shared DashMaps) | 10h |
| Proving/verification benchmarks | Measure client-side proving time and network-side verification time under realistic circuit size — this determines UX viability | 8h |

**Sub-total**: ~46h / ~1 week

### Tier 1 total: ~300h (~7.5 weeks solo, less with parallel work across S1/S3 and S2)

---

## Part 5: Testing conventions to follow

Match the existing codebase's conventions exactly — don't introduce a
second style:
- Inline `#[cfg(test)] mod tests` blocks in every source file.
- Factory helpers follow the `make_*` naming pattern already used
  elsewhere.
- Concurrent tests use `std::thread::spawn` with `Arc`-shared DashMaps, as
  `registry.rs`'s existing concurrent tests do.
- Use a `StubDataProvider`-equivalent for circuit/proof tests — don't
  require a live proving run for every unit test; only the dedicated
  benchmark and integration tests should run full proving.
- Run `cargo test --workspace --lib` before every commit; all existing 267
  tests must continue passing alongside new ones.

---

## Part 6: Tier 2 — private contract execution (read, don't build yet)

If asked to extend this to hide contract *logic and state*, not just value
transfer: this is a fundamentally different and much harder problem. The
Executor's job — running arbitrary contract bytecode — would need to happen
inside a provable virtual machine, where the prover shows "I ran this
program correctly on hidden state and produced this hidden new state,"
without revealing the program's intermediate execution trace or (depending
on requirements) the program itself.

This is comparable to what a handful of specialized L2 protocols (Aztec is
the closest real-world example) have spent years building, and it is not a
natural extension of the Tier 1 work above — it requires a general-purpose
zk-VM or a constrained contract language that compiles to circuits, an
entirely different proving architecture (likely STARK-based for
transparent, scalable proving of general computation, rather than the
Halo2 approach above which is tuned for a fixed, small set of circuits),
and materially different quorum/finalization logic.

**If this is requested**: push back on scoping it as a roadmap line item
with an hour estimate. Recommend it be treated as a separate research
initiative with its own feasibility study, after Tier 1 has shipped and its
lessons (proving performance, circuit review process, operational
experience with the nullifier set) can inform whether Tier 2 is worth
pursuing at all.

---

## Part 7: Risks and open questions to flag back to the project owner

- **Circuit correctness review**: zk circuit bugs are a different risk
  class from ordinary Rust bugs — a subtle constraint error can silently
  allow invalid proofs to verify. Budget for external review or audit
  before this goes anywhere near real value, the same way you'd treat the
  existing security-audit findings (wire framing, nonce, RNG issues) noted
  elsewhere in this project's history.
- **Proving performance on constrained devices**: client-side proving with
  Halo2 can take seconds; confirm this is acceptable for the intended
  wallet UX before committing to the architecture.
- **Viewing-key/compliance requirements** (Part 2.6) are a product
  decision, not just technical — confirm requirements before finalizing
  the note-encryption scheme, since retrofitting viewing keys later is
  disruptive (as it was for Zcash).
- **Anonymity set size in practice**: a shared pool only provides real
  privacy once enough real usage flows through it. Flag this as a
  bootstrapping/adoption concern, not just an engineering one.
