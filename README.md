# pneumatic_core

**Pneumatic** is a Rust implementation of a proof-of-stake blockchain protocol for distributed worker node networks. Each token is its own blockchain (a per-token block lattice) driven by a four-role pipeline — **Sentinel** (validate & route), **Executor** (execute), **Finalizer** (optimistically finalize), **Committer** (commit & epochs) — with stake-weighted deterministic leader election, per-transaction finalizer routing, executor sharding, optimistic finality with conflict-only quorum, a hybrid post-quantum crypto stack (Ed25519·ML-DSA-44 signatures, X25519·ML-KEM-768 key exchange), Reticulum Network Stack (RNS) as the inter-node transport, and a Tier-1 shielded ZK stack (halo2) for private value transfer.

[![Rust](https://img.shields.io/badge/Rust-2021-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

---

## Quick Start

```bash
cargo check              # Verify compilation
cargo build              # Build all workspace crates
cargo build -p pneumatic_node_server   # The deployment binary (`node-server`)
cargo test --workspace --lib   # 968 lib tests across 7 crates
cargo test --workspace         # 835 passed / 37 ignored (lib + integration + doc)
cargo test <filter>      # Run a single test, e.g. cargo test leader_selector
```

Live-proving and live-RNS tests are `#[ignore]`d by design (they are benchmarks, not regression tests); they run on demand: `cargo test --workspace -- --ignored`.

## Workspace Structure

```
pneumatic_core/
├── src/                  # pneumatic_core — core protocol library (no binary)
├── sentinel/             # pneumatic_sentinel — transaction validation & routing
├── executor/             # pneumatic_executor — contract execution
├── finalizer/            # pneumatic_finalizer — quorum & block building
├── committer/            # pneumatic_committer — chain commitment & epochs
├── node-server/          # pneumatic_node_server — composite runtime (binary `node-server`)
├── prover/               # pneumatic_prover — client-side halo2 proving (shielded Tier-1)
├── tests/                # workspace integration tests (pipeline, transport, shielded)
├── plans/                # shielded implementation plan & roadmap
├── infinite-brain/       # project memory vault — design decisions, facts, roadmap status
├── TASKS.md              # implementation checklist
├── CLAUDE.md             # development guidance
└── Cargo.toml            # workspace root + core crate config
```

### Key Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1.44.2 | Async runtime |
| `dashmap` | 7.0.0-rc0 | Concurrent registries (nodes, transactions, candidates) |
| `moka` | 0.12.10 | TTL-backed message dedup cache |
| `ed25519-dalek` | 2.0 | Classical half of hybrid signatures |
| `pqcrypto-mldsa` / `pqcrypto-mlkem` | 0.1.x | Post-quantum half (ML-DSA-44, ML-KEM-768) |
| `ring` / `sha2` | 0.17 / 0.10 | SHA-256 hashing |
| `aes-gcm` + `x25519-dalek` + `hkdf` | 0.11 / 3.0 / 0.12 | Hybrid encryption: DH + HKDF-SHA256 → AES-256-GCM |
| `halo2_proofs` | =0.3.5 (pinned) | Shielded Action circuit (proving/verification) |
| `rns-net` / `rns-crypto` / `rns-core` | =0.7.0 / 0.1.9 / 0.1.16 (pinned) | RNS inter-node transport |
| `serde` / `serde_json` / `rmp-serde` | 1.0 | JSON + MsgPack wire serialization |
| `rand` | 0.8 | Deterministic stake-weighted selection (StdRng seeded from SHA-256) |

Security-sensitive externals (`rns-*`, `halo2_proofs`, `pasta_curves`, `ff`) are exact-pinned in the workspace `Cargo.toml` — a version bump is an API-migration event, not a routine update.

## Architecture

### Module Map (pneumatic_core)

Top-level modules in `src/lib.rs` (with sub-packages where marked):

| Module | Responsibility |
|--------|---------------|
| `node` | Node types (Full/Light), registry types (Committer, Sentinel, Executor, Finalizer, Archiver), registration protocol |
| `node::registry` | `NodeRegistry` — per-type DashMap directories, binding-signed Register/RegisterAck, broadcast |
| `conns` | TCP/UDS trait families (`Connection`/`Sender`/`Stream`/`Listener`), length-prefixed framing — the legacy/local layer |
| `conns::factories` | `ConnFactory` — creates Senders, Listeners, Connections for TCP and UDS |
| `rns` | **Production inter-node wire** — `RnsNetwork` wrapper, `RnsConnection`, `NodeIdentity` dual keystore (RNS + Ed25519), destination routing, DoS guard |
| `server` | `ThreadPool` — hybrid sync+async worker pool |
| `config` | `Config` — loads `config.json` + per-environment specs from `/env/` |
| `environment` | `EnvironmentMetadata` — quorum, crypto provider, block validators, gas `CostModel` |
| `data` | `DataProvider` trait (MsgPack over UDS/TCP to a local data service); `StubDataProvider` for tests; epoch stake-snapshot persistence |
| `crypto` | `AsymCryptoProvider` — hybrid (N = N+1) Ed25519·ML-DSA-44 sign/verify and X25519·ML-KEM-768 hybrid encryption; `HashProvider` (SHA-256) |
| `encoding` | JSON and MsgPack serialization helpers |
| `auth` | Envelope authentication (C1: credit only keys proven by the envelope signature) |
| `tokens` | `Token` (embeds its own `Blockchain`), `BlockValidator` trait, `TokenFactory` (minting) |
| `blocks` | `Block` and `Blockchain` — append-only per-token chain with hash chaining; `BlockFactory` canonical hashing |
| `transactions` | `Transaction`, `SignedTransaction`, `TransactionCommit`, explicit `TransactionState` machine, `ShieldedTransaction` |
| `validation` | `TransactionValidationSpec` / `BlockValidatorSpec` traits + name-keyed registries; SelfSigned, Executed, Shielded specs |
| `registry` | `PendingTransactionRegistry` (in-flight tx state + used nonces), `NullifierRegistry` (shielded), `TransactionSignatureRegistry` |
| `epoch` | `Epoch`, `StakeSet`/`ExecutorSet`, `LeaderSelector`, `BlockProposer`, `CandidateRegistry`, `deterministic_select()` / `deterministic_select_shard()`, `EpochSnapshotCache<T>`, `resolve_block_conflict()` |
| `action_router` | `IActionRouter` — per-action nonce/gas/stake gating and dispatch |
| `gossiper` | Verify-then-dedup fan-out (content-keyed TTL cache) + `send_to_type` broadcast |
| `messages` | Wire `Message` struct (action + body + optional `StakeSet`), ack helpers |
| `logging` | `Logger` trait with `FileLogger` (file-locked append writes) |
| `user` | `User` with `fuel_balance` and `stake` |
| `errors` | `PneumaticError`, `ValidationFailureReason`, `TransactionRiskFactor`, `ReconciledSignatures` |
| `shielded` | Shielded Tier-1: `poseidon` (Poseidon1/Pallas), `note` (Pedersen commitments), `tree` (incremental Merkle, depth 32), `circuit` (halo2 Action circuit), `verify` (network-side verifier, cached vk), `roots`, `pool_view` |

### Node Roles

| Type | Role |
|------|------|
| **Sentinel** | Gatekeeper — fail-closed sender auth, gas + spec validation; self-signed tokens route direct to Committer, standard txs to Executor; deterministic per-transaction finalizer assignment (stake snapshots, executor-shard aware) |
| **Executor** | Contract execution — preloads data, runs backpressure-bounded execution, signs the result hash, sends a `Sign` vote to the assigned finalizer |
| **Finalizer** | Optimistic commit — first authenticated executor signature finalizes immediately; quorum machinery exists for conflict resolution and shielded stakes; builds `Block` chained to the token's chain tip |
| **Committer** | Terminal node — commits blocks (conflict detection + slashing at commit), stake-weighted quorum gossip, epoch loop (staking, reconciliation, leader proposal), archiver distribution |
| **Archiver** | Block distribution recipient |

### Consensus Flow

```
Sender → Sentinel → ─────────────────────────────────────────────→ Committer
                       │
                       ├─ SelfSigned token: validate → direct commit (skips Executor + Finalizer)
                       │
                       └─ Standard token:
                           Executor (execute + hash, shard-aware) →
                           Finalizer (first sig → optimistic block) →
                           Committer (commit to token chain)
```

**Transaction state machine:** `Pending → Preloaded → Validated → Executing → Finalizing → Committed`, with `Failed` reachable from any stage. Transitions are explicit via the `TransactionState` enum; a `PendingTransaction` holds an atomic lock count against premature collection during multi-stage transit.

**Epoch-based consensus:** time-bounded epochs; leader via stake-weighted deterministic selection (`SHA-256`-seeded walk over the sorted stake set, domain-separated and tip-bound); `EpochBoundaryDetector` for expiry; `resolve_block_conflict()` for competing proposals (higher stake wins, hash tie-break, same-proposer double-sign → slash).

**Optimistic finality:** standard tokens commit on the first valid executor signature + finalizer signature. Blocks start `Optimistic` and upgrade to `Confirmed` when the Committer observes stake-weighted quorum (voting stake ≥ total stake × quorum %, default 67%) via the `BlockFinalized` / `BlockConfirmed` / `BlockQuorumReached` gossip protocol. The 2/3 quorum is a dispute mechanism, not a happy-path gate.

**Deterministic per-transaction routing:** at each epoch boundary the Committer freezes the `StakeSet` (and `ExecutorSet`) via `DataProvider`; every node routes each transaction to its own finalizer via `deterministic_select(stakers, tx_id, epoch)` and to an executor shard via `deterministic_select_shard()`. A generic `EpochSnapshotCache<T>` (local tier → DataProvider tier; peer tier reserved) backs both, invalidated together on epoch advance.

**Gas model:** `CostModel` carries `base_cost`, `global_min_stake`, `admin_public_key`, `admin_tax_percentage`, and per-action multipliers (Process 1.0, Preload 2.0, Sign 1.5); `verify_gas()` checks `fuel_balance` before execution; gas is deducted at commit.

### Executor Operational Limits

The Executor bounds its resource use on three axes so a single misbehaving transaction or engine cannot starve the node (there is no unbounded resource path):

- **Backpressure (`max_in_flight`):** concurrent executions are capped at `max_in_flight` (a constructor parameter). A preload that would exceed the cap is rejected with `ExecutorError::AtCapacity` rather than queued, so worker slots and memory stay bounded.
- **Wall-clock backstop (`PNEUMATIC_EXECUTOR_TIMEOUT_SECS`):** every execution is wrapped in a `tokio::time::timeout` (default **5 s**, configurable via `PNEUMATIC_EXECUTOR_TIMEOUT_SECS` in whole seconds). The contract engine runs on a **blocking thread** so a stuck engine cannot block the async worker; when the backstop fires the transaction fails with `ExecutionTimeout` and its backpressure slot is freed (no hang, no leaked slot).
- **Gas cap:** a transaction's `gas_limit` caps the engine's `gas_used`; `gas_limit == 0` means **no cap**. Payload bytes feed the gas meter (3-stage model: static weight → payload → dynamic execution).
- **Panic isolation:** the engine is invoked under `std::panic::catch_unwind`; a panicking engine fails the transaction with `ContractExecutionFailed` instead of unwinding the worker task.

### Cryptography

- **Signatures (hybrid, N = N+1):** Ed25519 (64 B) + ML-DSA-44 (key + signature) concatenated — both halves must verify. Wire size ≈ 3,796 B.
- **Key exchange (hybrid):** X25519 + ML-KEM-768, HKDF-SHA256 to an AES-256-GCM key, random 96-bit nonce. Ciphertext ≈ 2,332 B for empty plaintext: `[ephemeral PK][nonce][ciphertext + 16 B GCM tag]`.
- **Hashing:** SHA-256 (`ring`/`sha2`). **Shielded:** Poseidon1 over Pallas Fp, Pedersen note commitments, depth-32 incremental Merkle tree, halo2 Action circuit (no trusted setup).

### Wire Protocol

- **Framing:** 4-byte big-endian length header + MsgPack (rmp-serde) payload; `MAX_FRAME_SIZE` = 16 MB enforced before allocation.
- **Inter-node transport:** RNS is the production wire for all role-to-role traffic (identity keystore, binding signatures, destination routing, announce-based discovery, control/data plane split, 4-thread worker pool, inbound DoS guard). Direct RNS packets cap at ~481 B, so full-size `Message` frames (≈3.8 KB with hybrid signatures) ride RNS resource transfer.
- **Local/legacy:** the TCP/UDS `conns` families remain for local service channels (data service: UDS-first, TCP loopback :55555 fallback). Port-per-registry-type: Committer 42001/50000, Sentinel 42002/50001, Executor 42003/50002, Finalizer 42004/50003, Archiver 42005/50004.

### Wire Actions

The `Message.action` string drives routing at every role; inbound handlers authenticate the envelope first (C1: credit only keys proven by the envelope signature + role gate — never self-reported body keys).

| Action | Producer → Consumer | Meaning |
|--------|---------------------|---------|
| `Process` | Sender → Sentinel | New transaction entering the pipeline |
| `Preload` | Sentinel → Executor / Finalizer | Preload transaction data (ordered before the vote) |
| `Sign` | Executor → Finalizer | Standard execution vote (inner signature over the result hash) |
| `SignShielded` | Sentinel → Finalizer | Shielded transaction (canonical tx bytes; skips the Executor) |
| `ShieldedVote` | Finalizer → Finalizer | Stake-weighted shielded quorum vote |
| `Commit` | Finalizer → Committer | `TransactionCommit` — submit a built block |
| `Confirm` / `Reject` | Finalizer → Sentinel | Per-transaction outcome; rejection triggers deterministic reassignment |
| `DistributeToken` / `DistributeBlock` | Committer → Committer / Archiver | Token and block distribution |
| `EpochReconcile` | Committer (self) | Epoch-boundary reconciliation trigger |
| `BlockFinalized` | Finalizer → all | Block + full `StakeSet` broadcast (quorum gossip start) |
| `BlockConfirmed` | Peer → Committer | Stake-weighted vote (block hash + sender key) |
| `BlockQuorumReached` | Committer → all | Quorum status broadcast (block → `Confirmed`) |
| `Register` / `RegisterAck` | Peer → NodeRegistry | Binding-signed registration with stake gates |

### Configuration

Node behavior is environment-driven: `Config::build` loads `config.json` plus every JSON spec in `/env/` into per-environment `EnvironmentMetadata` — partitions, quorum percentage, crypto provider, gas `CostModel`, and the validation/block-validator spec lists. Invalid specs fail boot (fail-closed). A missing environment metadata is a hard error in the composite runtime, since no node can run without it.

### Shielded Stack (Tier-1)

Private value transfer is implemented in `src/shielded/` (network side) and the `prover` crate (client side):

- **Notes:** Pedersen commitments over Pallas Ep (four hash-to-curve generators); a spend produces a one-way `nullifier = Poseidon1(spend_key, rho)` — consensus deduplicates nullifiers, so a double-spend is an invalid block.
- **Pool:** a global shielded pool (one pool for all tokens) of note commitments, tracked by a depth-32 incremental Merkle tree; validation requires the claimed Merkle root to exist in `MerkleRootState` history within a recency window.
- **Circuit:** a halo2 `ActionCircuit` proves spend authorization, Merkle membership of spent notes, well-formed output commitments, and value balance — construction fails closed on any malformed input. No trusted setup; the network verifier (`verify`) caches `keygen_vk` and verifies in milliseconds.
- **Proving is client-side** (the `prover` crate: key management, note building, circuit assembly, scan); workers only verify. A `ShieldedValidationSpec` runs four fail-closed gates (structural, nullifier membership, root freshness, proof verification) on the sentinel/finalizer path; the Committer applies pool updates atomically at commit (idempotent replay, first-spent-wins).

## Design Decisions

The protocol's architecture decision records (ADR-001…ADR-010) live in the [infinite-brain vault](infinite-brain/) as first-class decision nodes, with full context and rationale. Summary:

| ADR | Decision |
|-----|----------|
| 001 | Trait-based abstraction over inheritance for all pluggable components |
| 002 | DashMap for concurrent registry state (per-shard locking, read-heavy) |
| 003 | Deterministic leader election — SHA-256-seeded RNG over a sorted stake set |
| 004 | Deterministic per-transaction routing (tx-id seed) over a frozen stake snapshot |
| 005 / 010 | Optimistic finality — first executor sig finalizes; quorum repurposed as conflict-only dispute |
| 006 | Stake snapshots persisted in DataProvider, not block headers |
| 007 | Sentinel is the routing authority, not a consensus node |
| 008 | Conflict = same `(token_id, previous_hash)`, different hash; higher stake wins; same-proposer double-sign slashes both |
| 009 | Executor sharding with per-epoch Fisher-Yates rotation (stake-balanced round-robin) |

Full rationale: `infinite-brain/decisions/decision-*.md` (indexed in `infinite-brain/_system/INDEX.md`).

## Composite Node-Server Runtime

A single process — `pneumatic_node_server` (binary `node-server`) — *is* the deployment runtime. The four role crates install as **role-plugins**; RNS stays the external wire. Three in-process layers:

- **`RoleSelector`** — admits a role only when the node's own stake meets both the protocol floor and the per-type floor (`config::meets_minimum_stake`); fail-closed on zero stake; re-evaluated each epoch; `select_primary()` for single-role bootstrap (Finalizer > Executor > Sentinel > Committer).
- **`RoleDispatcher`** — routes an inbound `Message` by `action` to the single installed role that owns it (`RoleHandler`/`RoleHost` traits). Fail-closed: unknown actions raise `RoleError::UnknownAction` (logged, never dropped); an action owned by two roles raises `RoleError::AmbiguousAction`.
- **`NodeServer`** — owns the shared DI bundle and drives the lifecycle: epoch polling (`poll_and_advance`), role-set recomputation, background coordinator; RNS `on_packet` is bridged into `RoleDispatcher::dispatch`.

## Development

### Adding a New Validation Spec

1. Implement the `TransactionValidationSpec` trait (`validate()`, `calculate_risk()`, `name()`)
2. Implement the `BlockValidatorSpec` trait for block-level validation
3. Register via `ValidationSpecRegistry::register()` (or `register_defaults()`); unregistered specs fail closed

### Adding a New Node Type Handler

1. Add the action string to the `match` in the relevant role's message handler
2. Implement the handler — authenticate the envelope (C1), register the transaction, transition state, route via the gossiper
3. Add tests covering success and error paths

### Testing Conventions

- Inline `#[cfg(test)] mod tests` blocks in every source file; factory helpers follow the `make_*` pattern
- `StubDataProvider` for unit tests (in-memory, pre-loaded data); concurrent tests use `std::thread::spawn` with `Arc`-shared DashMaps
- Live-proving / live-RNS tests are `#[ignore]`d (benchmark-only) — see `pattern-cfg-test-proving` in the vault
- Test filter: `cargo test <module>::tests::<name>`

### Running All Tests

```bash
cargo test --workspace --lib
# 968 lib tests: 654 core + 109 committer + 33 executor + 61 finalizer + 39 node-server + 15 prover + 57 sentinel
cargo test --workspace
# 988 passed / 37 ignored: lib + integration (pipeline, transport, shielded) + doc tests
```

## Roadmap

The phase-by-phase checklist (Phases 0–10: foundation, the four worker pipelines, optimistic finality, deterministic routing, sharding, quorum gossip, RNS transport, production readiness, security audit remediation) is tracked in [TASKS.md](TASKS.md); dated status snapshots live in the vault (`infinite-brain/notes/`).

**Landed:** all foundation + worker phases, RNS transport (Phase 10), security-audit fixes SA_01–SA_08, hybrid PQ crypto (Phase 7), composite node-server runtime (Phases 1–7), shielded Tier-1 (S1.1–S6 — private value transfer with halo2 proofs, feature-complete), and **complete executor contract execution** (pluggable `ContractEngine` registry — `Transfer`/`Spec`/`Wasm` engines, per-transaction gas + wall-clock backstop, panic isolation, on-chain deploy, W3 Wasm storage, upgrade governance, ADR-016 Model X cross-contract calls) — verified end-to-end (7 composite pipeline tests + 2 cross-executor determinism tests; 988-test workspace suite green).

**Outstanding:** production readiness (rustdoc, operator runbook, observability, deployment infra — Phase 8) and the remaining test-gap tail in TASKS.md (e.g. `DefaultDataProvider` wire-format tests).

## Project Memory: Infinite Brain

This repo's institutional memory is the Obsidian-compatible vault at `infinite-brain/` (82+ typed nodes: pillars, decisions, concepts, facts, tasks, events, questions). Two standing duties for every working session:

1. **Orient via the vault** before architecture, protocol, crypto, or "what state is X" work — read `infinite-brain/_system/INDEX.md`, then follow edges. If a vault claim and the code disagree, the code wins — fix the node.
2. **Update the vault** after significant changes (architecture, protocol decisions, roadmap/phase status, baselines): update affected nodes, add nodes for genuinely new concepts, keep `_system/INDEX.md` in sync, and append a log node to `infinite-brain/logs/`.

Operating rules: `infinite-brain/_system/AGENTS.md`.

## Contributing

1. Fork the repository
2. Create a feature branch from `main`
3. Implement changes with inline `#[cfg(test)]` tests
4. Run `cargo test --workspace` — all tests must pass
5. Update TASKS.md for completed items; update the vault per the standing protocol above
6. Open a pull request
