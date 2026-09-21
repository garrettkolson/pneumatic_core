---
name: infinite-brain-convert-note
description: Ingest raw documents into an Infinite Brain vault as atomic typed nodes with typed edges, updating the index. Use when asked to convert notes, ingest a document into the brain, decompose content into knowledge nodes, or process raw/ files.
---

# Convert Note

Ingest raw content from `raw/` into atomic typed nodes in the Infinite Brain vault.

## When to use
- "Convert this note into the vault"
- "Ingest this document"
- "Process the files in raw/"
- "Decompose this into knowledge nodes"

## Inputs
- **target**: specific file in `raw/`, or "all"
- If no `raw/` content is provided, ask the user for the source content or path.

## Steps
1. Pick target file(s) in `raw/`. Treat source files as immutable — never edit them.
2. Read `_system/INDEX.md` first to know existing node ids (avoid collisions).
3. Decompose the document into atomic nodes — one concept per node, 50-300 words each.
4. For each node:
   - Classify with exactly one of the 16 content types (see `_system/NODE-TYPES.md`).
   - Assign a unique `id` in `type-descriptive-slug` format.
   - Populate all frontmatter fields per `_system/FRONTMATTER-SCHEMA.md`.
   - Wire to at least one other node using the 10 edge types (`_system/EDGE-TYPES.md`).
   - Place in the correct folder: `type: concept` → `concepts/<id>.md`.
5. Write each node file.
6. Append each new node's row to `_system/INDEX.md` under the correct type section.
7. Move processed source file(s) from `raw/` to `raw/processed/`.
8. Write a log node:
   - File: `logs/log-convert-note-YYYYMMDD-HHmmss.md`
   - Frontmatter: `operation: convert-note`, affected nodes, one-sentence summary
   - Body: 30-80 word description of what was ingested
   - Log nodes are never indexed and never decayed.

## Log node schema (8 fields)
```yaml
id: log-op-YYYYMMDD-HHmmss
type: log
operation: convert-note | query-vault | organize-vault | vault-health | manual
date: YYYY-MM-DD
namespace: <namespace>
summary: "one sentence"
affected_nodes: [node-ids]
tags: []
```

## Rules
- One concept per node — never merge distinct concepts. Err toward more atomic nodes.
- Every node must have at least one edge.
- Summaries under 200 characters.
- Confidence must reflect actual certainty (1.0 only for code-verified facts).
- `visibility` defaults to `namespace`; `public` only for genuinely universal claims.
- `staleness_signal` must be a specific observable condition, not a vague phrase.
- Tags: 2-8 kebab-case strings, consistent with existing vault vocabulary.
- If a node contradicts an existing node, wire a `contradicts` edge — do not delete either.
