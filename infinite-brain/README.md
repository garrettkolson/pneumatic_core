# Infinite Brain — pneumatic vault

Knowledge-graph vault for the **pneumatic** blockchain repo, built on the [Infinite Brain](https://github.com/JotaSXBR/obsidian-infinite-brain) system (default branch `master`).

## What this is

Every file in a type folder is a **node**: a single atomic idea with YAML frontmatter (id, type, namespace, summary, confidence, edges…). Edges in the frontmatter wire nodes into a typed graph. Agents navigate the graph to answer questions; humans browse it in Obsidian (this folder is a valid Obsidian vault root — open `infinite-brain/` as the vault folder).

## Map

| Path | Contents |
|------|----------|
| `_system/AGENTS.md` | **Start here** — how agents use this vault |
| `_system/INDEX.md` | Master index: one row per node |
| `_system/NODE-TYPES.md` / `EDGE-TYPES.md` / `FRONTMATTER-SCHEMA.md` | The three schemas |
| `_system/WORKFLOWS.md` / `LOCAL-TYPES.md` | Workflows + custom types (`module`) |
| `_system/_prompts/` | The 4 prompt templates (Create, Convert, Query, Organize) |
| `_templates/` | Node template |
| `pillars/` | Top-level pillars (block lattice, shielded value transfer) |
| `decisions/` | Key design decisions (client-side proving, halo2, notes, pool, nullifiers) |
| `concepts/` | Protocol + shielded + worker + network concepts |
| `facts/` | Code-verified facts (workspace layout, wire protocol, PQ crypto, test baseline…) |
| `patterns/` | Reusable patterns (fail-closed, pinned deps, ignore-gated proving tests) |
| `events/` | Landed milestones (S5.2 finalizer wiring, RNS e2e, node-server composite, PQ hybrid) |
| `tasks/` | Open next-step tasks (S5.3/S5.4 + top non-shielded tasks) |
| `questions/` | Open questions (viewing keys / compliance) |
| `hypotheses/`, `notes/` | Unverified claims / status snapshots |
| `sources/` | Documents the vault is derived from (roadmap, plan, PROTOCOL_CHANGES, TASKS, audit, README ADRs) |
| `logs/` | Audit trail of vault operations (never indexed, never decayed) |
| `raw/` → `raw/processed/` | Ingestion queue for the convert-note workflow |
| `bookmarks/`, `contacts/`, `references/`, `custom/` | Sparsely used types; `custom/` holds the `module` type |

## Agent skills

Claude skills live in `.claude/skills/infinite-brain-*` (init-vault, convert-note, query-vault, organize-vault, vault-health). Skills operate on this folder: vault root = `infinite-brain/`.

## Invariants

- Every non-log node has a row in `_system/INDEX.md`.
- Node ids are globally unique; every node has ≥1 edge.
- Code-derived claims cite `file:line` and carry `confidence: 1.0` only when verified against the code at `verified_at`.
- `logs/` is append-only.
